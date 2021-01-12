use crate::{
    device::queue::qbuf::get_free::GetFreeBufferError,
    device::queue::qbuf::get_free::GetFreeOutputBuffer,
    device::queue::qbuf::QBuffer,
    device::queue::{
        self, dqbuf::DQBuffer, BuffersAllocated, CreateQueueError, FormatBuilder, Queue, QueueInit,
    },
    device::Stream,
    device::{
        poller::{DeviceEvent, PollEvents, Poller},
        queue::direction::{Capture, Output},
        AllocatedQueue,
    },
    device::{queue::qbuf::CaptureQueueable, DeviceOpenError, TryDequeue},
    ioctl::DQBufError,
    ioctl::GFmtError,
    ioctl::{self, subscribe_event},
    ioctl::{BufferCapabilities, FormatFlags, StreamOnError},
    memory::{MMAPHandle, UserPtrHandle},
    Format,
};

use queue::qbuf::get_free::GetFreeCaptureBuffer;
use std::{
    io,
    path::Path,
    sync::{atomic::AtomicUsize, Arc},
    thread::JoinHandle,
};
use thiserror::Error;

use crate::device::{Device, DeviceConfig};

// Trait implemented by all states of the decoder.
pub trait DecoderState {}

pub struct Decoder<S: DecoderState> {
    device: Arc<Device>,
    state: S,
}

pub struct AwaitingOutputFormat {
    output_queue: Queue<Output, QueueInit>,
    capture_queue: Queue<Capture, QueueInit>,
}
impl DecoderState for AwaitingOutputFormat {}

#[derive(Debug, Error)]
pub enum DecoderOpenError {
    #[error("Error while opening device")]
    DeviceOpenError(#[from] DeviceOpenError),
    #[error("Error while creating queue")]
    CreateQueueError(#[from] CreateQueueError),
    #[error("Specified device is not a stateful decoder")]
    NotAStatefulDecoder,
}

impl Decoder<AwaitingOutputFormat> {
    pub fn open(path: &Path) -> Result<Self, DecoderOpenError> {
        let config = DeviceConfig::new().non_blocking_dqbuf();
        let device = Arc::new(Device::open(path, config)?);

        // Check that the device is indeed a stateful decoder.
        let capture_queue = Queue::get_capture_mplane_queue(device.clone())?;
        let output_queue = Queue::get_output_mplane_queue(device.clone())?;

        // On a decoder, the OUTPUT formats are compressed, but the CAPTURE ones are not.
        // Return an error if our device does not satisfy these conditions.
        output_queue
            .format_iter()
            .find(|fmt| fmt.flags.contains(FormatFlags::COMPRESSED))
            .and(
                capture_queue
                    .format_iter()
                    .find(|fmt| !fmt.flags.contains(FormatFlags::COMPRESSED)),
            )
            .ok_or(DecoderOpenError::NotAStatefulDecoder)
            .map(|_| ())?;

        // A stateful decoder won't expose the requests capability on the OUTPUT
        // queue, a stateless one will.
        if output_queue
            .get_capabilities()
            .contains(BufferCapabilities::SUPPORTS_REQUESTS)
        {
            return Err(DecoderOpenError::NotAStatefulDecoder);
        }

        Ok(Decoder {
            device,
            state: AwaitingOutputFormat {
                output_queue,
                capture_queue,
            },
        })
    }

    // TODO apply same change to encoder!
    pub fn set_output_format<F>(mut self, f: F) -> anyhow::Result<Decoder<AwaitingOutputBuffers>>
    where
        F: FnOnce(FormatBuilder) -> anyhow::Result<()>,
    {
        let builder = self.state.output_queue.change_format()?;
        f(builder)?;

        Ok(Decoder {
            device: self.device,
            state: AwaitingOutputBuffers {
                output_queue: self.state.output_queue,
                capture_queue: self.state.capture_queue,
            },
        })
    }
}

pub struct AwaitingOutputBuffers {
    output_queue: Queue<Output, QueueInit>,
    capture_queue: Queue<Capture, QueueInit>,
}
impl DecoderState for AwaitingOutputBuffers {}

impl Decoder<AwaitingOutputBuffers> {
    pub fn allocate_output_buffers(
        self,
        num_buffers: usize,
    ) -> Result<Decoder<OutputBuffersAllocated>, queue::RequestBuffersError> {
        let output_queue = self
            .state
            .output_queue
            .request_buffers::<Vec<UserPtrHandle<Vec<u8>>>>(num_buffers as u32)?;

        Ok(Decoder {
            device: self.device,
            state: OutputBuffersAllocated {
                output_queue,
                capture_queue: self.state.capture_queue,
                poll_wakeups_counter: None,
            },
        })
    }
}

pub struct OutputBuffersAllocated {
    output_queue: Queue<Output, BuffersAllocated<Vec<UserPtrHandle<Vec<u8>>>>>,
    capture_queue: Queue<Capture, QueueInit>,
    poll_wakeups_counter: Option<Arc<AtomicUsize>>,
}
impl DecoderState for OutputBuffersAllocated {}

#[derive(Debug, Error)]
pub enum StartDecoderError {
    #[error("IO error")]
    IoError(#[from] io::Error),
    #[error("Cannot subscribe to decoder event")]
    SubscribeEventError(#[from] ioctl::SubscribeEventError),
    #[error("Error while starting the output queue")]
    StreamOnError(#[from] StreamOnError),
}

impl Decoder<OutputBuffersAllocated> {
    pub fn set_poll_counter(mut self, poll_wakeups_counter: Arc<AtomicUsize>) -> Self {
        self.state.poll_wakeups_counter = Some(poll_wakeups_counter);
        self
    }

    pub fn start<InputDoneCb, OutputReadyCb, SetCaptureFormatCb>(
        self,
        input_done_cb: InputDoneCb,
        output_ready_cb: OutputReadyCb,
        set_capture_format_cb: SetCaptureFormatCb,
    ) -> Result<Decoder<Decoding<InputDoneCb, OutputReadyCb, SetCaptureFormatCb>>, StartDecoderError>
    where
        InputDoneCb: Fn(&mut Vec<UserPtrHandle<Vec<u8>>>),
        OutputReadyCb: FnMut(DQBuffer<Capture, Vec<MMAPHandle>>) + Send + 'static,
        SetCaptureFormatCb: Fn(FormatBuilder) -> anyhow::Result<()> + Send + 'static,
    {
        // We are interested in all resolution change events.
        subscribe_event(
            &*self.device,
            ioctl::EventType::SourceChange,
            ioctl::SubscribeEventFlags::empty(),
        )?;

        let mut output_poller = Poller::new(Arc::clone(&self.device))?;
        output_poller.enable_event(DeviceEvent::OutputReady)?;

        let mut decoder_thread = DecoderThread::new(
            &self.device,
            self.state.capture_queue,
            output_ready_cb,
            set_capture_format_cb,
        )?;

        if let Some(counter) = &self.state.poll_wakeups_counter {
            output_poller.set_poll_counter(Arc::clone(counter));
            decoder_thread.set_poll_counter(Arc::clone(counter));
        }

        let handle = std::thread::Builder::new()
            .name("V4L2 Decoder".into())
            .spawn(move || decoder_thread.run())?;

        self.state.output_queue.stream_on()?;

        Ok(Decoder {
            device: self.device,
            state: Decoding {
                output_queue: self.state.output_queue,
                input_done_cb,
                output_poller,
                handle,
            },
        })
    }
}

pub struct Decoding<InputDoneCb, OutputReadyCb, SetCaptureFormatCb>
where
    InputDoneCb: Fn(&mut Vec<UserPtrHandle<Vec<u8>>>),
    OutputReadyCb: FnMut(DQBuffer<Capture, Vec<MMAPHandle>>) + Send,
    SetCaptureFormatCb: Fn(FormatBuilder) -> anyhow::Result<()>,
{
    output_queue: Queue<Output, BuffersAllocated<Vec<UserPtrHandle<Vec<u8>>>>>,
    input_done_cb: InputDoneCb,
    output_poller: Poller,

    handle: JoinHandle<DecoderThread<OutputReadyCb, SetCaptureFormatCb>>,
}
impl<InputDoneCb, OutputReadyCb, SetCaptureFormatCb> DecoderState
    for Decoding<InputDoneCb, OutputReadyCb, SetCaptureFormatCb>
where
    InputDoneCb: Fn(&mut Vec<UserPtrHandle<Vec<u8>>>),
    OutputReadyCb: FnMut(DQBuffer<Capture, Vec<MMAPHandle>>) + Send,
    SetCaptureFormatCb: Fn(FormatBuilder) -> anyhow::Result<()>,
{
}

type DequeueOutputBufferError = DQBufError<DQBuffer<Output, Vec<UserPtrHandle<Vec<u8>>>>>;

impl<InputDoneCb, OutputReadyCb, SetCaptureFormatCb>
    Decoder<Decoding<InputDoneCb, OutputReadyCb, SetCaptureFormatCb>>
where
    InputDoneCb: Fn(&mut Vec<UserPtrHandle<Vec<u8>>>),
    OutputReadyCb: FnMut(DQBuffer<Capture, Vec<MMAPHandle>>) + Send,
    SetCaptureFormatCb: Fn(FormatBuilder) -> anyhow::Result<()>,
{
    pub fn num_output_buffers(&self) -> usize {
        self.state.output_queue.num_buffers()
    }

    pub fn get_output_format(&self) -> Result<Format, GFmtError> {
        self.state.output_queue.get_format()
    }

    pub fn stop(self) -> Result<(), ioctl::DecoderCmdError> {
        // TODO if the CAPTURE queue is not running, we cannot dequeue the
        // LAST buffer. In this case we need another way to stop the thread.
        ioctl::decoder_cmd(&*self.device, ioctl::DecoderCommand::Stop)?;

        let decoding_thread = self.state.handle.join().unwrap();

        match &decoding_thread.capture_queue {
            CaptureQueue::Decoding(queue) => {
                queue.stream_off().unwrap();
            }
            _ => todo!(),
        }

        Ok(())
    }

    /// Attempts to dequeue and release output buffers that the driver is done with.
    fn dequeue_output_buffers(&self) -> Result<(), DequeueOutputBufferError> {
        let output_queue = &self.state.output_queue;

        while output_queue.num_queued_buffers() > 0 {
            match output_queue.try_dequeue() {
                Ok(mut buf) => {
                    // unwrap() is safe here as we just dequeued the buffer.
                    (self.state.input_done_cb)(&mut buf.take_handles().unwrap());
                }
                Err(DQBufError::NotReady) => break,
                // TODO buffers with the error flag set should not result in
                // a fatal error!
                Err(e) => return Err(e),
            }
        }

        Ok(())
    }

    // Make this thread sleep until at least one OUTPUT buffer is ready to be
    // obtained through `try_get_buffer()`, dequeuing buffers if necessary.
    fn wait_for_output_buffer(&mut self) -> Result<(), GetBufferError> {
        for event in self.state.output_poller.poll(None)? {
            match event {
                PollEvents::DEVICE_OUTPUT => {
                    self.dequeue_output_buffers()?;
                }
                _ => panic!("Unexpected return from OUTPUT queue poll!"),
            }
        }

        Ok(())
    }

    /// Returns a V4L2 buffer to be filled with a frame to encode, waiting for
    /// one to be available if needed.
    ///
    /// If all allocated buffers are currently queued, this method will wait for
    /// one to be available.
    pub fn get_buffer(&mut self) -> Result<OutputBuffer, GetBufferError> {
        let output_queue = &self.state.output_queue;

        // If all our buffers are queued, wait until we can dequeue some.
        if output_queue.num_queued_buffers() == output_queue.num_buffers() {
            self.wait_for_output_buffer()?;
        }

        self.try_get_free_buffer()
    }
}

#[derive(Debug, Error)]
pub enum GetBufferError {
    #[error("Error while dequeueing buffer")]
    DequeueError(#[from] DequeueOutputBufferError),
    #[error("Error during poll")]
    PollError(#[from] io::Error),
    #[error("Error while obtaining buffer")]
    GetFreeBufferError(#[from] GetFreeBufferError),
}

pub type OutputBuffer<'a> =
    QBuffer<'a, Output, Vec<UserPtrHandle<Vec<u8>>>, Vec<UserPtrHandle<Vec<u8>>>>;

impl<'a, InputDoneCb, OutputReadyCb, SetCaptureFormatCb>
    GetFreeOutputBuffer<'a, Vec<UserPtrHandle<Vec<u8>>>, GetBufferError>
    for Decoder<Decoding<InputDoneCb, OutputReadyCb, SetCaptureFormatCb>>
where
    InputDoneCb: Fn(&mut Vec<UserPtrHandle<Vec<u8>>>),
    OutputReadyCb: FnMut(DQBuffer<Capture, Vec<MMAPHandle>>) + Send,
    SetCaptureFormatCb: Fn(FormatBuilder) -> anyhow::Result<()>,
{
    type Queueable = OutputBuffer<'a>;

    fn try_get_free_buffer(&'a self) -> Result<Self::Queueable, GetBufferError> {
        while self.state.output_queue.try_dequeue().is_ok() {}
        Ok(self.state.output_queue.try_get_free_buffer()?)
    }
}

/*
enum CaptureState {
    AwaitingResolution {
        capture_queue: Queue<Capture, QueueInit>,
        // Poller used to detect the initial resolution change event.
        poller: Poller,
    },
    Running {
        capture_queue: Queue<Capture, BuffersAllocated<Vec<MMAPHandle>>>,
        // Poller used to be notified of CAPTURE buffers being ready to dequeue
        // or re-enqueue after being given to the client.
        poller: Poller,
    },
}
*/

enum CaptureQueue {
    AwaitingResolution(Queue<Capture, QueueInit>),
    Decoding(Queue<Capture, BuffersAllocated<Vec<MMAPHandle>>>),
}

struct DecoderThread<OutputReadyCb, SetCaptureFormatCb>
where
    OutputReadyCb: FnMut(DQBuffer<Capture, Vec<MMAPHandle>>) + Send,
{
    device: Arc<Device>,
    capture_queue: CaptureQueue,
    poller: Poller,
    output_ready_cb: OutputReadyCb,
    set_capture_format_cb: SetCaptureFormatCb,
}

#[derive(Debug, Error)]
enum UpdateCaptureError {
    #[error("Error while obtaining CAPTURE format")]
    GFmt(#[from] ioctl::GFmtError),
    #[error("Error while setting CAPTURE format")]
    SFmt(#[from] ioctl::SFmtError),
    #[error("Error while requesting CAPTURE buffers")]
    RequestBuffers(#[from] queue::RequestBuffersError),
    #[error("Error while streaming CAPTURE queue")]
    StreamOn(#[from] ioctl::StreamOnError),
}

#[derive(Debug, Error)]
enum ProcessEventsError {
    #[error("Error while dequeueing event")]
    DQEvent(#[from] ioctl::DQEventError),
    #[error("Error while requesting buffers")]
    RequestBuffers(#[from] queue::RequestBuffersError),
    #[error("Error while updating CAPTURE format")]
    UpdateCapture(#[from] UpdateCaptureError),
}

impl<OutputReadyCb, SetCaptureFormatCb> DecoderThread<OutputReadyCb, SetCaptureFormatCb>
where
    OutputReadyCb: FnMut(DQBuffer<Capture, Vec<MMAPHandle>>) + Send,
    for<'a> Queue<Capture, BuffersAllocated<Vec<MMAPHandle>>>:
        GetFreeCaptureBuffer<'a, Vec<MMAPHandle>>,
    SetCaptureFormatCb: Fn(FormatBuilder) -> anyhow::Result<()>,
{
    fn new(
        device: &Arc<Device>,
        capture_queue: Queue<Capture, QueueInit>,
        output_ready_cb: OutputReadyCb,
        set_capture_format_cb: SetCaptureFormatCb,
    ) -> io::Result<Self> {
        let mut poller = Poller::new(Arc::clone(device))?;
        // Start by only listening to V4L2 events in order to catch the initial
        // resolution change.
        poller.enable_event(DeviceEvent::V4L2Event)?;

        let decoder_thread = DecoderThread {
            device: Arc::clone(&device),
            capture_queue: CaptureQueue::AwaitingResolution(capture_queue),
            poller,
            output_ready_cb,
            set_capture_format_cb,
        };

        Ok(decoder_thread)
    }

    fn set_poll_counter(&mut self, poll_wakeups_counter: Arc<AtomicUsize>) {
        self.poller.set_poll_counter(poll_wakeups_counter);
    }

    fn update_capture_resolution(self) -> Result<Self, UpdateCaptureError> {
        let mut capture_queue = match self.capture_queue {
            // Initial resolution
            CaptureQueue::AwaitingResolution(queue) => queue,
            // Dynamic resolution change
            CaptureQueue::Decoding(queue) => {
                // TODO remove unwrap.
                // TODO must do complete flush sequence before this...
                queue.stream_off().unwrap();
                queue.free_buffers().unwrap().queue
            }
        };

        (self.set_capture_format_cb)(capture_queue.change_format()?).unwrap();

        let capture_queue = capture_queue.request_buffers::<Vec<MMAPHandle>>(4)?;
        println!("Allocated {} buffers", capture_queue.num_buffers());

        // TODO use two closures, one to set the format, another one to decide
        // the number of buffers, given the minimum number of buffers for the
        // stream (need control support for that).

        // Reconfigure poller to listen to capture buffers being ready.
        let mut poller = self.poller;
        poller.enable_event(DeviceEvent::CaptureReady).unwrap();
        poller.disable_event(DeviceEvent::V4L2Event).unwrap();

        capture_queue.stream_on()?;

        let mut new_self = Self {
            capture_queue: CaptureQueue::Decoding(capture_queue),
            poller,
            ..self
        };

        new_self.enqueue_capture_buffers();

        Ok(new_self)
    }

    // A resolution change event will potentially morph the capture queue
    // from the Init state to BuffersAllocated - thus we take full ownership
    // of self and return a new object.
    fn process_events(mut self) -> Result<Self, ProcessEventsError> {
        loop {
            // TODO what if we used an iterator here?
            let event = match ioctl::dqevent(&*self.device) {
                Ok(event) => event,
                Err(ioctl::DQEventError::NotReady) => break,
                Err(e) => return Err(e.into()),
            };

            match event {
                ioctl::Event::SrcChangeEvent(changes) => {
                    if changes.contains(ioctl::SrcChanges::RESOLUTION) {
                        println!("Got resolution change event!");
                        self = self.update_capture_resolution()?;
                    }
                }
            }
        }

        Ok(self)
    }

    fn process_capture_buffer(&mut self) -> bool {
        match &mut self.capture_queue {
            CaptureQueue::Decoding(capture_queue) => {
                if let Ok(mut cap_buf) = capture_queue.try_dequeue() {
                    let is_last = cap_buf.data.flags.contains(ioctl::BufferFlags::LAST);
                    let is_empty = cap_buf.data.planes[0].bytesused == 0;

                    // Add a drop callback to the dequeued buffer so we
                    // re-queue it as soon as it is dropped.
                    let cap_waker = Arc::clone(self.poller.get_waker());
                    cap_buf.add_drop_callback(move |_dqbuf| {
                        // Intentionally ignore the result here.
                        let _ = cap_waker.wake();
                        // TODO how about a way to immediately re-queue the buffer
                        // in the drop callback? That way we don't need to interrupt
                        // polling on the device.
                        // Actually, the buffer is back into the free list when
                        // we are here! So we can completely do that, provided
                        // we have a reference to the queue. If we use a sync::Weak
                        // pointer to the queue we should be able to do it. And
                        // when buffers are reallocated the Arc to the queue needs
                        // to be destroyed anyway, so the weak pointer cannot be
                        // upgraded!
                        // We already have a weak reference in the fuse, and a weak
                        // pointer to the device in the dqbuffer, can't we reuse that?
                        // What we need: a Weak reference to the queue, passed to the callback.
                        // Then we can call try_get_buffer() from here using the
                        // buffer index as argument, and requeue the buffer using
                        // the handles from the dqbuffer!
                        // Or maybe that won't work. We shouldn't be able to call streamoff while
                        // we hold a QBuffer, and that would allow this to happen if the destructor
                        // runs in another thread while we attempt to stop the queue.
                        // Maybe have a DQBuffer::requeue() method that requeues the
                        // buffer as is after removing the plane handles and data?
                        // TODO streamoff and try_get*buffer() should be &mut self to avoid calling
                        // streamoff while we hold a qbuffer? What happens if we do? -> Nothing since
                        // the buffer is not queued and we can queue it if the queue is streamed off!
                        // That's no problem at all.
                        // But wait - we need to change the poll state when requeuing buffers anyway,
                        // so we need to wake up from the poll...
                    });

                    // Empty buffers do not need to be passed to the client.
                    if !is_empty {
                        (self.output_ready_cb)(cap_buf);
                    }

                    // Last buffer of the stream? Time for us to terminate.
                    // TODO but not if there is a resolution change event.
                    // in this case we need to perform a DRC.
                    if is_last {
                        return true;
                    }
                } else {
                    // TODO we should not crash here.
                    panic!("Expected a CAPTURE buffer but none available!");
                }
            }
            // TODO replace with something more elegant.
            _ => panic!(),
        }

        false
    }

    fn run(mut self) -> Self {
        'polling: loop {
            match &self.capture_queue {
                CaptureQueue::AwaitingResolution(_capture_queue) => {
                    // Here we only check for the resolution change event and
                    // set a bool if we get it.
                }
                CaptureQueue::Decoding(_capture_queue) => {
                    // Here we process buffers as usual while looking for the
                    // LAST buffer and checking if we need to res change (and
                    // set the boolean if we do.
                }
            }

            // Check if we need to change resolution and do so taking ownership
            // over the capture queue and returning a new one.

            // TODO redesign PollEvents as an iterator so we can check events
            // one by one and detect unexpected ones.

            if let CaptureQueue::Decoding(capture_queue) = &self.capture_queue {
                match capture_queue.num_queued_buffers() {
                    // If there are no buffers on the CAPTURE queue, poll() will return
                    // immediately with EPOLLERR and we would loop indefinitely.
                    // Prevent this by temporarily disabling polling the CAPTURE queue
                    // in such cases.
                    0 => {
                        self.poller
                            .disable_event(DeviceEvent::CaptureReady)
                            .unwrap();
                    }
                    // If device polling was disabled and we have buffers queued, we
                    // can reenable it as poll will now wait for a CAPTURE buffer to
                    // be ready for dequeue.
                    _ => {
                        self.poller.enable_event(DeviceEvent::CaptureReady).unwrap();
                    }
                }
            }

            // TODO remove this unwrap.
            let events = self.poller.poll(None).unwrap();

            if events.contains(PollEvents::DEVICE_CAPTURE) {
                let do_exit = self.process_capture_buffer();
                if do_exit {
                    break 'polling;
                }
            }

            // TODO only do this while we are waiting for the initial resolution?
            // Afterwards we can dequeue events when we get a LAST buffer.
            if events.contains(PollEvents::DEVICE_EVENT) {
                self = self.process_events().unwrap();
            }

            // TODO when doing DRC, it can happen that buffers from the previous
            // resolution are released and trigger this. We need to make the
            // old waker a no-op (maybe by reinitializing it to a new file?)
            // before streaming the CAPTURE queue off. Maybe allocate a new Poller
            // as we morph our queue type?
            if events.contains(PollEvents::WAKER) {
                // Requeue all available CAPTURE buffers.
                self.enqueue_capture_buffers();
            }
        }

        self
    }

    fn enqueue_capture_buffers(&mut self) {
        if let CaptureQueue::Decoding(capture_queue) = &self.capture_queue {
            while let Ok(buffer) = capture_queue.try_get_free_buffer() {
                buffer.queue_with_handles(Default::default()).unwrap();
            }
        }
    }
}
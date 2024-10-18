use alloc::{collections::BTreeMap,
            format,
            string::{String, ToString},
            vec::Vec};
use core::{option::NoneError, slice, str};

use core_io::{Error as IoError, Write};
use cslice::AsCSlice;
use dma::{Error as DmaError, Manager as DmaManager};
use io::{Cursor, ProtoWrite};
use ksupport::{eh_artiq, kernel, rpc, rtio};
use libboard_artiq::{drtio_routing::RoutingTable,
                     drtioaux,
                     drtioaux_proto::{PayloadStatus, MASTER_PAYLOAD_MAX_SIZE},
                     pl::csr};
use libboard_zynq::{time::Milliseconds, timer::GlobalTimer};
use libcortex_a9::sync_channel::Receiver;
use log::warn;
use routing::{Router, SliceMeta, Sliceable};

#[derive(Debug, Clone, PartialEq)]
enum KernelState {
    Absent,
    Loaded,
    Running,
    MsgAwait {
        max_time: Option<Milliseconds>,
        id: u32,
        tags: Vec<u8>,
    },
    MsgSending,
    SubkernelAwaitLoad,
    SubkernelAwaitFinish {
        max_time: Option<Milliseconds>,
        id: u32,
    },
    DmaUploading,
    DmaPendingPlayback {
        id: u32,
        timestamp: u64,
    },
    DmaPendingAwait {
        id: u32,
        timestamp: u64,
        max_time: Milliseconds,
    },
    DmaAwait {
        max_time: Milliseconds,
    },
    SubkernelRetrievingException {
        destination: u8,
    },
}

#[derive(Debug)]
pub enum Error {
    Load(String),
    KernelNotFound,
    Unexpected(String),
    NoMessage,
    AwaitingMessage,
    SubkernelIoError,
    DrtioError,
    KernelException(Sliceable),
    DmaError(DmaError),
}

impl From<NoneError> for Error {
    fn from(_: NoneError) -> Error {
        Error::KernelNotFound
    }
}

impl From<IoError> for Error {
    fn from(_value: IoError) -> Error {
        Error::SubkernelIoError
    }
}

impl From<DmaError> for Error {
    fn from(value: DmaError) -> Error {
        Error::DmaError(value)
    }
}

impl From<()> for Error {
    fn from(_: ()) -> Error {
        Error::NoMessage
    }
}

impl From<drtioaux::Error> for Error {
    fn from(_value: drtioaux::Error) -> Error {
        Error::DrtioError
    }
}

macro_rules! unexpected {
    ($($arg:tt)*) => (return Err(Error::Unexpected(format!($($arg)*))));
}

/* represents interkernel messages */
struct Message {
    count: u8,
    id: u32,
    data: Vec<u8>,
}

#[derive(PartialEq)]
enum OutMessageState {
    NoMessage,
    MessageBeingSent,
    MessageSent,
    MessageAcknowledged,
}

/* for dealing with incoming and outgoing interkernel messages */
struct MessageManager {
    out_message: Option<Sliceable>,
    out_state: OutMessageState,
    in_queue: Vec<Message>,
    in_buffer: Option<Message>,
}

// Per-run state
struct Session {
    id: u32,
    kernel_state: KernelState,
    last_exception: Option<Sliceable>,   // exceptions raised locally
    external_exception: Option<Vec<u8>>, // exceptions from sub-subkernels
    messages: MessageManager,
    source: u8, // which destination requested running the kernel
    subkernels_finished: Vec<(u32, Option<u8>)>,
}

impl Session {
    pub fn new(id: u32) -> Session {
        Session {
            id: id,
            kernel_state: KernelState::Absent,
            last_exception: None,
            external_exception: None,
            messages: MessageManager::new(),
            source: 0,
            subkernels_finished: Vec::new(),
        }
    }

    fn running(&self) -> bool {
        match self.kernel_state {
            KernelState::Absent | KernelState::Loaded => false,
            _ => true,
        }
    }
}

#[derive(Debug)]
struct KernelLibrary {
    library: Vec<u8>,
    complete: bool,
}

pub struct Manager<'a> {
    kernels: BTreeMap<u32, KernelLibrary>,
    session: Session,
    control: &'a mut kernel::Control,
    cache: BTreeMap<String, Vec<i32>>,
    last_finished: Option<SubkernelFinished>,
}

pub struct SubkernelFinished {
    pub id: u32,
    pub with_exception: bool,
    pub exception_source: u8,
    pub source: u8,
}

impl MessageManager {
    pub fn new() -> MessageManager {
        MessageManager {
            out_message: None,
            out_state: OutMessageState::NoMessage,
            in_queue: Vec::new(),
            in_buffer: None,
        }
    }

    pub fn handle_incoming(
        &mut self,
        status: PayloadStatus,
        id: u32,
        length: usize,
        data: &[u8; MASTER_PAYLOAD_MAX_SIZE],
    ) {
        // called when receiving a message from master
        if status.is_first() {
            self.in_buffer = None;
        }
        match self.in_buffer.as_mut() {
            Some(message) => message.data.extend(&data[..length]),
            None => {
                self.in_buffer = Some(Message {
                    count: data[0],
                    id: id,
                    data: data[1..length].to_vec(),
                });
            }
        };
        if status.is_last() {
            // when done, remove from working queue
            self.in_queue.push(self.in_buffer.take().unwrap());
        }
    }

    pub fn was_message_acknowledged(&mut self) -> bool {
        match self.out_state {
            OutMessageState::MessageAcknowledged => {
                self.out_state = OutMessageState::NoMessage;
                true
            }
            _ => false,
        }
    }

    pub fn get_outgoing_slice(&mut self, data_slice: &mut [u8; MASTER_PAYLOAD_MAX_SIZE]) -> Option<SliceMeta> {
        if self.out_state != OutMessageState::MessageBeingSent {
            return None;
        }
        let meta = self.out_message.as_mut()?.get_slice_master(data_slice);
        if meta.status.is_last() {
            // clear the message slot
            self.out_message = None;
            // notify kernel with a flag that message is sent
            self.out_state = OutMessageState::MessageSent;
        }
        Some(meta)
    }

    pub fn ack_slice(&mut self) -> bool {
        // returns whether or not there's more to be sent
        match self.out_state {
            OutMessageState::MessageBeingSent => true,
            OutMessageState::MessageSent => {
                self.out_state = OutMessageState::MessageAcknowledged;
                false
            }
            _ => {
                warn!("received unsolicited SubkernelMessageAck");
                false
            }
        }
    }

    pub fn accept_outgoing(
        &mut self,
        id: u32,
        self_destination: u8,
        destination: u8,
        message: Vec<u8>,
        routing_table: &RoutingTable,
        rank: u8,
        router: &mut Router,
    ) -> Result<(), Error> {
        self.out_message = Some(Sliceable::new(destination, message));

        let mut data_slice: [u8; MASTER_PAYLOAD_MAX_SIZE] = [0; MASTER_PAYLOAD_MAX_SIZE];
        self.out_state = OutMessageState::MessageBeingSent;
        let meta = self.get_outgoing_slice(&mut data_slice).unwrap();
        router.route(
            drtioaux::Packet::SubkernelMessage {
                source: self_destination,
                destination: destination,
                id: id,
                status: meta.status,
                length: meta.len as u16,
                data: data_slice,
            },
            routing_table,
            rank,
            self_destination,
        );
        Ok(())
    }

    pub fn get_incoming(&mut self, id: u32) -> Option<Message> {
        for i in 0..self.in_queue.len() {
            if self.in_queue[i].id == id {
                return Some(self.in_queue.remove(i));
            }
        }
        None
    }
}

impl<'a> Manager<'_> {
    pub fn new(control: &mut kernel::Control) -> Manager {
        Manager {
            kernels: BTreeMap::new(),
            session: Session::new(0),
            control: control,
            cache: BTreeMap::new(),
            last_finished: None,
        }
    }

    pub fn add(&mut self, id: u32, status: PayloadStatus, data: &[u8], data_len: usize) -> Result<(), Error> {
        let kernel = match self.kernels.get_mut(&id) {
            Some(kernel) => {
                if kernel.complete || status.is_first() {
                    // replace entry
                    self.kernels.remove(&id);
                    self.kernels.insert(
                        id,
                        KernelLibrary {
                            library: Vec::new(),
                            complete: false,
                        },
                    );
                    self.kernels.get_mut(&id)?
                } else {
                    kernel
                }
            }
            None => {
                self.kernels.insert(
                    id,
                    KernelLibrary {
                        library: Vec::new(),
                        complete: false,
                    },
                );
                self.kernels.get_mut(&id)?
            }
        };
        kernel.library.extend(&data[0..data_len]);

        kernel.complete = status.is_last();
        Ok(())
    }

    pub fn running(&self) -> bool {
        self.session.running()
    }

    pub fn get_current_id(&self) -> Option<u32> {
        match self.running() {
            true => Some(self.session.id),
            false => None,
        }
    }

    pub fn run(&mut self, source: u8, id: u32, timestamp: u64) -> Result<(), Error> {
        if self.session.kernel_state != KernelState::Loaded || self.session.id != id {
            self.load(id)?;
        }
        self.session.kernel_state = KernelState::Running;
        self.session.source = source;
        unsafe {
            csr::cri_con::selected_write(2);
        }

        rtio::at_mu(timestamp as i64);
        self.control.tx.send(kernel::Message::StartRequest);
        Ok(())
    }

    pub fn message_handle_incoming(
        &mut self,
        status: PayloadStatus,
        id: u32,
        length: usize,
        slice: &[u8; MASTER_PAYLOAD_MAX_SIZE],
    ) {
        if !self.running() {
            return;
        }
        self.session.messages.handle_incoming(status, id, length, slice);
    }

    pub fn message_get_slice(&mut self, slice: &mut [u8; MASTER_PAYLOAD_MAX_SIZE]) -> Option<SliceMeta> {
        if !self.running() {
            return None;
        }
        self.session.messages.get_outgoing_slice(slice)
    }

    pub fn message_ack_slice(&mut self) -> bool {
        if !self.running() {
            warn!("received unsolicited SubkernelMessageAck");
            return false;
        }
        self.session.messages.ack_slice()
    }

    pub fn load(&mut self, id: u32) -> Result<(), Error> {
        if self.session.id == id && self.session.kernel_state == KernelState::Loaded {
            return Ok(());
        }
        if !self.kernels.get(&id)?.complete {
            return Err(Error::KernelNotFound);
        }
        self.session = Session::new(id);
        self.control.restart();

        self.control
            .tx
            .send(kernel::Message::LoadRequest(self.kernels.get(&id)?.library.clone()));
        let reply = self.control.rx.recv();
        match reply {
            kernel::Message::LoadCompleted => Ok(()),
            kernel::Message::LoadFailed => Err(Error::Load("kernel load failed".to_string())),
            _ => Err(Error::Load(format!(
                "unexpected kernel CPU reply to load request: {:?}",
                reply
            ))),
        }
    }

    pub fn exception_get_slice(&mut self, data_slice: &mut [u8; MASTER_PAYLOAD_MAX_SIZE]) -> SliceMeta {
        match self.session.last_exception.as_mut() {
            Some(exception) => exception.get_slice_master(data_slice),
            None => SliceMeta {
                destination: 0,
                len: 0,
                status: PayloadStatus::FirstAndLast,
            },
        }
    }

    fn kernel_stop(&mut self) {
        self.session.kernel_state = KernelState::Absent;
        unsafe {
            csr::cri_con::selected_write(0);
        }
    }

    fn runtime_exception(&mut self, cause: Error) {
        let raw_exception: Vec<u8> = Vec::new();
        let mut writer = Cursor::new(raw_exception);
        match write_exception(
            &mut writer,
            &[Some(eh_artiq::Exception {
                id: 11, // SubkernelError, defined in ksupport
                message: format!("in subkernel id {}: {:?}", self.session.id, cause).as_c_slice(),
                param: [0, 0, 0],
                file: file!().as_c_slice(),
                line: line!(),
                column: column!(),
                function: format!("subkernel id {}", self.session.id).as_c_slice(),
            })],
            &[eh_artiq::StackPointerBacktrace {
                stack_pointer: 0,
                initial_backtrace_size: 0,
                current_backtrace_size: 0,
            }],
            &[],
            0,
        ) {
            Ok(_) => self.session.last_exception = Some(Sliceable::new(0, writer.into_inner())),
            Err(_) => error!("Error writing exception data"),
        }
        self.kernel_stop();
    }

    pub fn ddma_finished(&mut self, error: u8, channel: u32, timestamp: u64) {
        if let KernelState::DmaAwait { .. } = self.session.kernel_state {
            self.control.tx.send(kernel::Message::DmaAwaitRemoteReply {
                timeout: false,
                error: error,
                channel: channel,
                timestamp: timestamp,
            });
            self.session.kernel_state = KernelState::Running;
        }
    }

    pub fn ddma_nack(&mut self) {
        // for simplicity treat it as a timeout...
        if let KernelState::DmaAwait { .. } = self.session.kernel_state {
            self.control.tx.send(kernel::Message::DmaAwaitRemoteReply {
                timeout: true,
                error: 0,
                channel: 0,
                timestamp: 0,
            });
            self.session.kernel_state = KernelState::Running;
        }
    }

    pub fn ddma_remote_uploaded(&mut self, succeeded: bool) -> Option<(u32, u64)> {
        // returns a tuple of id, timestamp in case a playback needs to be started immediately
        if !succeeded {
            self.kernel_stop();
            self.runtime_exception(Error::DmaError(DmaError::UploadFail));
        }
        let res = match self.session.kernel_state {
            KernelState::DmaPendingPlayback { id, timestamp } => {
                self.session.kernel_state = KernelState::Running;
                Some((id, timestamp))
            }
            KernelState::DmaPendingAwait {
                id,
                timestamp,
                max_time,
            } => {
                self.session.kernel_state = KernelState::DmaAwait { max_time: max_time };
                Some((id, timestamp))
            }
            KernelState::DmaUploading => {
                self.session.kernel_state = KernelState::Running;
                None
            }
            _ => None,
        };
        res
    }

    pub fn process_kern_requests(
        &mut self,
        router: &mut Router,
        routing_table: &RoutingTable,
        rank: u8,
        destination: u8,
        dma_manager: &mut DmaManager,
        timer: &GlobalTimer,
    ) {
        if let Some(subkernel_finished) = self.last_finished.take() {
            info!(
                "subkernel {} finished, with exception: {}",
                subkernel_finished.id, subkernel_finished.with_exception
            );
            router.route(
                drtioaux::Packet::SubkernelFinished {
                    destination: subkernel_finished.source,
                    id: subkernel_finished.id,
                    with_exception: subkernel_finished.with_exception,
                    exception_src: subkernel_finished.exception_source,
                },
                &routing_table,
                rank,
                destination,
            );
        }

        if !self.running() {
            return;
        }

        match self.process_external_messages(router, routing_table, rank, destination, timer) {
            Ok(()) => (),
            Err(Error::AwaitingMessage) => return, // kernel still waiting, do not process kernel messages
            Err(Error::KernelException(exception)) => {
                self.session.last_exception = Some(exception);
                self.last_finished = Some(SubkernelFinished {
                    id: self.session.id,
                    with_exception: true,
                    exception_source: destination,
                    source: self.session.source,
                });
            }
            Err(e) => {
                error!("Error while running processing external messages: {:?}", e);
                self.runtime_exception(e);
                self.last_finished = Some(SubkernelFinished {
                    id: self.session.id,
                    with_exception: true,
                    exception_source: destination,
                    source: self.session.source,
                });
            }
        }

        match self.process_kern_message(router, routing_table, rank, destination, dma_manager, timer) {
            Ok(true) => {
                self.last_finished = Some(SubkernelFinished {
                    id: self.session.id,
                    with_exception: false,
                    exception_source: 0,
                    source: self.session.source,
                });
            }
            Ok(false) | Err(Error::NoMessage) => (),
            Err(Error::KernelException(exception)) => {
                self.session.last_exception = Some(exception);
                self.last_finished = Some(SubkernelFinished {
                    id: self.session.id,
                    with_exception: true,
                    exception_source: destination,
                    source: self.session.source,
                });
            }
            Err(e) => {
                error!("Error while running kernel: {:?}", e);
                self.runtime_exception(e);
                self.last_finished = Some(SubkernelFinished {
                    id: self.session.id,
                    with_exception: true,
                    exception_source: destination,
                    source: self.session.source,
                });
            }
        }
    }

    fn check_finished_kernels(
        &mut self,
        id: u32,
        router: &mut Router,
        routing_table: &RoutingTable,
        rank: u8,
        self_destination: u8,
    ) {
        for (i, (status, exception_source)) in self.session.subkernels_finished.iter().enumerate() {
            if *status == id {
                if exception_source.is_none() {
                    self.control.tx.send(kernel::Message::SubkernelAwaitFinishReply);
                    self.session.kernel_state = KernelState::Running;
                    self.session.subkernels_finished.swap_remove(i);
                } else {
                    let destination = exception_source.unwrap();
                    self.session.external_exception = Some(Vec::new());
                    self.session.kernel_state = KernelState::SubkernelRetrievingException {
                        destination: destination,
                    };
                    router.route(
                        drtioaux::Packet::SubkernelExceptionRequest {
                            source: self_destination,
                            destination: destination,
                        },
                        &routing_table,
                        rank,
                        self_destination,
                    );
                }
                break;
            }
        }
    }

    pub fn subkernel_load_run_reply(&mut self, succeeded: bool) {
        if self.session.kernel_state == KernelState::SubkernelAwaitLoad {
            self.control
                .tx
                .send(kernel::Message::SubkernelLoadRunReply { succeeded: succeeded });
            self.session.kernel_state = KernelState::Running;
        } else {
            warn!("received unsolicited SubkernelLoadRunReply");
        }
    }

    pub fn remote_subkernel_finished(&mut self, id: u32, with_exception: bool, exception_source: u8) {
        let exception_src = if with_exception { Some(exception_source) } else { None };
        self.session.subkernels_finished.push((id, exception_src));
    }

    pub fn received_exception(
        &mut self,
        exception_data: &[u8],
        last: bool,
        router: &mut Router,
        routing_table: &RoutingTable,
        rank: u8,
        self_destination: u8,
    ) {
        if let KernelState::SubkernelRetrievingException { destination } = self.session.kernel_state {
            self.session
                .external_exception
                .as_mut()
                .unwrap()
                .extend_from_slice(exception_data);
            if last {
                self.control
                    .tx
                    .send(kernel::Message::SubkernelError(kernel::SubkernelStatus::Exception(
                        self.session.external_exception.take().unwrap(),
                    )));
                self.session.kernel_state = KernelState::Running;
            } else {
                /* fetch another slice */
                router.route(
                    drtioaux::Packet::SubkernelExceptionRequest {
                        source: self_destination,
                        destination: destination,
                    },
                    routing_table,
                    rank,
                    self_destination,
                );
            }
        } else {
            warn!("Received unsolicited exception data");
        }
    }

    fn process_kern_message(
        &mut self,
        router: &mut Router,
        routing_table: &RoutingTable,
        rank: u8,
        self_destination: u8,
        dma_manager: &mut DmaManager,
        timer: &GlobalTimer,
    ) -> Result<bool, Error> {
        let reply = self.control.rx.try_recv()?;
        match reply {
            kernel::Message::KernelFinished(_async_errors) => {
                self.kernel_stop();
                dma_manager.cleanup(router, rank, self_destination, routing_table);
                return Ok(true);
            }
            kernel::Message::KernelException(exceptions, stack_pointers, backtrace, async_errors) => {
                error!("exception in kernel");
                for exception in exceptions {
                    error!("{:?}", exception.unwrap());
                }
                error!("stack pointers: {:?}", stack_pointers);
                error!("backtrace: {:?}", backtrace);
                let buf: Vec<u8> = Vec::new();
                let mut writer = Cursor::new(buf);
                match write_exception(&mut writer, exceptions, stack_pointers, backtrace, async_errors) {
                    Ok(()) => (),
                    Err(_) => error!("Error writing exception data"),
                }
                self.kernel_stop();
                return Err(Error::KernelException(Sliceable::new(0, writer.into_inner())));
            }
            kernel::Message::CachePutRequest(key, value) => {
                self.cache.insert(key, value);
            }
            kernel::Message::CacheGetRequest(key) => {
                const DEFAULT: Vec<i32> = Vec::new();
                let value = self.cache.get(&key).unwrap_or(&DEFAULT).clone();
                self.control.tx.send(kernel::Message::CacheGetReply(value));
            }

            kernel::Message::DmaPutRequest(recorder) => {
                // ddma is always used on satellites
                if let Ok(id) = dma_manager.put_record(recorder, self_destination) {
                    dma_manager.upload_traces(id, router, rank, self_destination, routing_table)?;
                    self.session.kernel_state = KernelState::DmaUploading;
                } else {
                    unexpected!("DMAError: found an unsupported call to RTIO devices on master")
                }
            }
            kernel::Message::DmaEraseRequest(name) => {
                dma_manager.erase_name(&name, router, rank, self_destination, routing_table);
            }
            kernel::Message::DmaGetRequest(name) => {
                let dma_meta = dma_manager.retrieve(self_destination, &name);
                self.control.tx.send(kernel::Message::DmaGetReply(dma_meta));
            }
            kernel::Message::DmaStartRemoteRequest { id, timestamp } => {
                if self.session.kernel_state != KernelState::DmaUploading {
                    dma_manager.playback_remote(
                        id as u32,
                        timestamp as u64,
                        router,
                        rank,
                        self_destination,
                        routing_table,
                    )?;
                } else {
                    self.session.kernel_state = KernelState::DmaPendingPlayback {
                        id: id as u32,
                        timestamp: timestamp as u64,
                    };
                }
            }
            kernel::Message::DmaAwaitRemoteRequest(_id) => {
                let max_time = timer.get_time() + Milliseconds(10000);
                self.session.kernel_state = match self.session.kernel_state {
                    // if we are still waiting for the traces to be uploaded, extend the state by timeout
                    KernelState::DmaPendingPlayback { id, timestamp } => KernelState::DmaPendingAwait {
                        id: id,
                        timestamp: timestamp,
                        max_time: max_time,
                    },
                    _ => KernelState::DmaAwait { max_time: max_time },
                };
            }

            kernel::Message::SubkernelMsgSend {
                id: _id,
                destination: msg_dest,
                data,
            } => {
                let msg_dest = msg_dest.or(Some(self.session.source)).unwrap();
                self.session.messages.accept_outgoing(
                    self.session.id,
                    self_destination,
                    msg_dest,
                    data,
                    routing_table,
                    rank,
                    router,
                )?;
                self.session.kernel_state = KernelState::MsgSending;
            }
            kernel::Message::SubkernelMsgRecvRequest { id, timeout, tags } => {
                let id = if id == -1 { self.session.id } else { id as u32 };
                let max_time = if timeout > 0 {
                    Some(timer.get_time() + Milliseconds(timeout as u64))
                } else {
                    None
                };
                self.session.kernel_state = KernelState::MsgAwait {
                    max_time: max_time,
                    id: id,
                    tags: tags,
                };
            }
            kernel::Message::SubkernelLoadRunRequest {
                id,
                destination: sk_destination,
                run,
                timestamp,
            } => {
                self.session.kernel_state = KernelState::SubkernelAwaitLoad;
                router.route(
                    drtioaux::Packet::SubkernelLoadRunRequest {
                        source: self_destination,
                        destination: sk_destination,
                        id: id,
                        run: run,
                        timestamp,
                    },
                    routing_table,
                    rank,
                    self_destination,
                );
            }

            kernel::Message::SubkernelAwaitFinishRequest { id, timeout } => {
                let max_time = if timeout > 0 {
                    Some(timer.get_time() + Milliseconds(timeout as u64))
                } else {
                    None
                };
                self.session.kernel_state = KernelState::SubkernelAwaitFinish {
                    max_time: max_time,
                    id: id,
                };
            }
            kernel::Message::UpDestinationsRequest(destination) => {
                self.control.tx.send(kernel::Message::UpDestinationsReply(
                    destination == (self_destination as i32),
                ));
            }
            _ => {
                unexpected!("unexpected message from core1 while kernel was running: {:?}", reply);
            }
        }
        Ok(false)
    }

    fn process_external_messages(
        &mut self,
        router: &mut Router,
        routing_table: &RoutingTable,
        rank: u8,
        self_destination: u8,
        timer: &GlobalTimer,
    ) -> Result<(), Error> {
        match &self.session.kernel_state {
            KernelState::MsgAwait { max_time, id, tags } => {
                if let Some(max_time) = *max_time {
                    if timer.get_time() > max_time {
                        self.control
                            .tx
                            .send(kernel::Message::SubkernelError(kernel::SubkernelStatus::Timeout));
                        self.session.kernel_state = KernelState::Running;
                        return Ok(());
                    }
                }
                if let Some(message) = self.session.messages.get_incoming(*id) {
                    self.control
                        .tx
                        .send(kernel::Message::SubkernelMsgRecvReply { count: message.count });
                    let tags = tags.clone();
                    self.session.kernel_state = KernelState::Running;
                    self.pass_message_to_kernel(&message, tags, timer)
                } else {
                    let id = *id;
                    self.check_finished_kernels(id, router, routing_table, rank, self_destination);
                    Err(Error::AwaitingMessage)
                }
            }
            KernelState::MsgSending => {
                if self.session.messages.was_message_acknowledged() {
                    self.session.kernel_state = KernelState::Running;
                    self.control.tx.send(kernel::Message::SubkernelMsgSent);
                    Ok(())
                } else {
                    Err(Error::AwaitingMessage)
                }
            }
            KernelState::SubkernelAwaitFinish { max_time, id } => {
                if let Some(max_time) = *max_time {
                    if timer.get_time() > max_time {
                        self.control
                            .tx
                            .send(kernel::Message::SubkernelError(kernel::SubkernelStatus::Timeout));
                        self.session.kernel_state = KernelState::Running;
                        return Ok(());
                    }
                }
                let id = *id;
                self.check_finished_kernels(id, router, routing_table, rank, self_destination);
                Ok(())
            }
            KernelState::SubkernelRetrievingException { .. } => Err(Error::AwaitingMessage),
            KernelState::DmaAwait { max_time } | KernelState::DmaPendingAwait { max_time, .. } => {
                if timer.get_time() > *max_time {
                    self.control.tx.send(kernel::Message::DmaAwaitRemoteReply {
                        timeout: true,
                        error: 0,
                        channel: 0,
                        timestamp: 0,
                    });
                    self.session.kernel_state = KernelState::Running;
                }
                Ok(())
            }
            _ => Ok(()),
        }
    }

    fn pass_message_to_kernel(&mut self, message: &Message, tags: Vec<u8>, timer: &GlobalTimer) -> Result<(), Error> {
        let mut reader = Cursor::new(&message.data);
        let mut current_tags: &[u8] = &tags;
        let mut i = message.count;
        loop {
            let slot = match recv_w_timeout(&mut self.control.rx, timer, 100)? {
                kernel::Message::RpcRecvRequest(slot) => slot,
                other => unexpected!("expected root value slot from core1, not {:?}", other),
            };
            let mut exception: Option<Sliceable> = None;
            let mut unexpected: Option<String> = None;
            let remaining_tags = rpc::recv_return(&mut reader, current_tags, slot, &mut |size| {
                if size == 0 {
                    0 as *mut ()
                } else {
                    self.control.tx.send(kernel::Message::RpcRecvReply(Ok(size)));
                    match recv_w_timeout(&mut self.control.rx, timer, 100) {
                        Ok(kernel::Message::RpcRecvRequest(slot)) => slot,
                        Ok(kernel::Message::KernelException(exceptions, stack_pointers, backtrace, async_errors)) => {
                            let buf: Vec<u8> = Vec::new();
                            let mut writer = Cursor::new(buf);
                            match write_exception(&mut writer, exceptions, stack_pointers, backtrace, async_errors) {
                                Ok(()) => {
                                    exception = Some(Sliceable::new(0, writer.into_inner()));
                                }
                                Err(_) => {
                                    unexpected = Some("Error writing exception data".to_string());
                                }
                            };
                            0 as *mut ()
                        }
                        other => {
                            unexpected = Some(format!("expected nested value slot from kernel CPU, not {:?}", other));
                            0 as *mut ()
                        }
                    }
                }
            })?;
            if let Some(exception) = exception {
                self.kernel_stop();
                return Err(Error::KernelException(exception));
            } else if let Some(unexpected) = unexpected {
                self.kernel_stop();
                unexpected!("{}", unexpected);
            }
            self.control.tx.send(kernel::Message::RpcRecvReply(Ok(0)));
            i -= 1;
            if i == 0 {
                break;
            } else {
                current_tags = remaining_tags;
            }
        }
        Ok(())
    }
}

fn write_exception<W>(
    writer: &mut W,
    exceptions: &[Option<eh_artiq::Exception>],
    stack_pointers: &[eh_artiq::StackPointerBacktrace],
    backtrace: &[(usize, usize)],
    async_errors: u8,
) -> Result<(), Error>
where
    W: Write + ?Sized,
{
    /* header */
    writer.write_bytes(&[0x5a, 0x5a, 0x5a, 0x5a, /*Reply::KernelException*/ 9])?;
    writer.write_u32(exceptions.len() as u32)?;
    for exception in exceptions.iter() {
        let exception = exception.as_ref().unwrap();
        writer.write_u32(exception.id)?;

        if exception.message.len() == usize::MAX {
            // exception with host string
            writer.write_u32(u32::MAX)?;
            writer.write_u32(exception.message.as_ptr() as u32)?;
        } else {
            let msg =
                str::from_utf8(unsafe { slice::from_raw_parts(exception.message.as_ptr(), exception.message.len()) })
                    .unwrap()
                    .replace(
                        "{rtio_channel_info:0}",
                        &format!(
                            "0x{:04x}:{}",
                            exception.param[0],
                            ksupport::resolve_channel_name(exception.param[0] as u32)
                        ),
                    );
            writer.write_string(&msg)?;
        }
        writer.write_u64(exception.param[0] as u64)?;
        writer.write_u64(exception.param[1] as u64)?;
        writer.write_u64(exception.param[2] as u64)?;
        writer.write_bytes(exception.file.as_ref())?;
        writer.write_u32(exception.line)?;
        writer.write_u32(exception.column)?;
        writer.write_bytes(exception.function.as_ref())?;
    }

    for sp in stack_pointers.iter() {
        writer.write_u32(sp.stack_pointer as u32)?;
        writer.write_u32(sp.initial_backtrace_size as u32)?;
        writer.write_u32(sp.current_backtrace_size as u32)?;
    }
    writer.write_u32(backtrace.len() as u32)?;
    for &(addr, sp) in backtrace {
        writer.write_u32(addr as u32)?;
        writer.write_u32(sp as u32)?;
    }
    writer.write_u8(async_errors as u8)?;
    Ok(())
}

fn recv_w_timeout(
    rx: &mut Receiver<'_, kernel::Message>,
    timer: &GlobalTimer,
    timeout: u64,
) -> Result<kernel::Message, Error> {
    let max_time = timer.get_time() + Milliseconds(timeout);
    while timer.get_time() < max_time {
        match rx.try_recv() {
            Err(_) => (),
            Ok(message) => return Ok(message),
        }
    }
    Err(Error::NoMessage)
}

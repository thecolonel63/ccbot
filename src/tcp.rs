use crate::{log, ChannelPair, Packet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc::UnboundedSender;

use anyhow::{anyhow, Result};

const TCP_PORT: u16 = 25687;

macro_rules! impl_next {
    ($ty:ty,$id:ident) => {
        fn $id(&mut self) -> Result<$ty> {
            let len = size_of::<$ty>();
            if self.read_cursor + len > self.write_cursor {
                return Err(anyhow!("Ran out of room while reading!"));
            }
            let data = <$ty>::from_be_bytes(self.data[self.read_cursor..self.read_cursor + len].try_into()?);
            self.read_cursor += len;
            Ok(data)
        }
    };
}

macro_rules! impl_put {
    ($ty:ty,$id:ident) => {
        fn $id(&mut self, val: $ty) -> Result<()> {
            let len = size_of::<$ty>();
            if self.write_cursor + len > BUFFER_SIZE {
                return Err(anyhow!("Ran into end of buffer while writing!"));
            }
            self.data[self.write_cursor..self.write_cursor + len].copy_from_slice(&val.to_be_bytes());
            self.write_cursor += len;
            Ok(())
        }
    };
}

pub async fn start_tcp(tx: UnboundedSender<ChannelPair<Packet>>) -> Result<()> {
    let listener = TcpListener::bind(format!("127.0.0.1:{TCP_PORT}")).await?;
    log!("Successfully started tcp listener on port {TCP_PORT}");
    loop {
        let (stream, _) = listener.accept().await?;
        let thread_tx = tx.clone();
        tokio::spawn(async move {
            if let Err(why) = handle_tcp_client(stream, thread_tx).await {
                log!("Error handling client: {why:?}");
            }
        });
    }
}

async fn handle_tcp_client(mut client: TcpStream, tx: UnboundedSender<ChannelPair<Packet>>) -> Result<()> {
    let mut local_pair = ChannelPair::new();

    let mut buf = Buffer::new();
    buf.read_from_tcp(&mut client).await?;
    let id = buf.next_u8()?;

    match id {
        0 => {
            let uuid = buf.next_string()?;
            let name = buf.next_string()?;
            tx.send(local_pair.entangle())?;
            local_pair.sender.send(Packet::ConnectQuery(name, uuid))?;
            let Packet::ConnectResponse(response) = local_pair.receiver.recv().await.ok_or(anyhow!("Main thread did not respond!"))? else { return Err(anyhow!("Unexpected packet received in tcp client!")) };

            buf.reset();
            buf.put_u8(0)?;
            buf.put_string(response)?;
            buf.write_to_tcp(&mut client).await?;
        }

        _ => {}
    }

    Ok(())
}

const BUFFER_SIZE: usize = 128;

struct Buffer {
    read_cursor: usize,
    write_cursor: usize,
    data: Box<[u8]>,
}

impl Buffer {
    fn new() -> Self {
        Self {
            read_cursor: 0,
            write_cursor: 0,
            data: vec![0u8; BUFFER_SIZE].into_boxed_slice(),
        }
    }

    fn reset(&mut self) {
        self.read_cursor = 0;
        self.write_cursor = 0;
    }

    async fn read_from_tcp(&mut self, stream: &mut TcpStream) -> Result<()> {
        self.reset();

        // Read the length as an integer
        stream.read_exact(&mut self.data[0..4]).await?;
        let len = u32::from_be_bytes(self.data[0..4].try_into()?) as usize;

        if len > BUFFER_SIZE {
            return Err(anyhow!("Attempted to read packet with length {len}!"));
        }

        stream.read_exact(&mut self.data[0..len]).await?;
        self.write_cursor += len;
        Ok(())
    }

    async fn write_to_tcp(&mut self, stream: &mut TcpStream) -> Result<()> {
        stream.write_all(&(self.write_cursor as u32).to_be_bytes()).await?;
        stream.write_all(&self.data[0..self.write_cursor]).await?;
        self.reset();
        Ok(())
    }

    impl_next!(u8, next_u8);
    impl_next!(u32, next_u32);

    fn next_string(&mut self) -> Result<String> {
        let len = self.next_u32()? as usize;
        if self.read_cursor + len > self.write_cursor {
            return Err(anyhow!("Ran out of room while reading!"));
        }
        let data = &self.data[self.read_cursor..self.read_cursor + len];
        self.read_cursor += len;
        Ok(String::from_utf8(Vec::from(data))?)
    }

    impl_put!(u8, put_u8);
    impl_put!(u32, put_u32);

    fn put_string(&mut self, val: String) -> Result<()> {
        let len = val.len();
        self.put_u32(len as u32)?;
        if self.write_cursor + len > BUFFER_SIZE {
            return Err(anyhow!("Ran into end of buffer while writing!"));
        }
        self.data[self.write_cursor..self.write_cursor + len].copy_from_slice(val.as_bytes());
        self.write_cursor += len;
        Ok(())
    }
}
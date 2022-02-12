use byteorder::{BigEndian, ReadBytesExt};
use multisock::{SocketAddr, Stream};
use ssh_agent::proto;
use ssh_agent::proto::public_key::PublicKey;
use ssh_agent::proto::signature;
use ssh_agent::proto::{from_bytes, to_bytes, Message};
use log::*;

use std::io::{Read, Write};
// use std::mem::size_of;
use std::net::Shutdown;

use super::error::RsshErr;

type ErrType = Box<dyn std::error::Error>;

pub struct AgentClient<'a> {
    addr: &'a str,
    stream: Option<Stream>,
}

static NET_RETRY_CNT: u32 = 3;

impl<'a> AgentClient<'a> {
    pub fn new(addr: &str) -> AgentClient {
        AgentClient { addr, stream: None }
    }

    fn read_message(stream: &mut Stream) -> Result<Message, ErrType> {
        let length = stream.read_u32::<BigEndian>()? as usize;
        debug!("read_message len={}", length);
        let mut buffer: Vec<u8> = vec![0; length as usize];
        stream.read_exact(buffer.as_mut_slice())?;
        trace!("Read {} bytes: {:02X?}", buffer.len(), buffer);
        let msg: Message = from_bytes(buffer.as_slice())?;
        Ok(msg)
    }

    fn write_message(stream: &mut Stream, msg: &Message) -> Result<(), ErrType> {
        let mut bytes = to_bytes(&to_bytes(msg)?)?;
        stream.write_all(&mut bytes)?;
        trace!("Written {} bytes: {:02X?}", bytes.len(), bytes);
        Ok(())
    }

    fn connect(&mut self) -> Result<(), ErrType> {
        let addr = if self.addr.starts_with('/') {
            String::from("unix:") + self.addr
        } else {
            String::from(self.addr)
        };
        let sockaddr: SocketAddr = addr.parse()?;
        if let Some(ref mut s) = self.stream {
            let _ = s.shutdown(Shutdown::Both);
            self.stream = None;
        }
        self.stream = Some(Stream::connect(&sockaddr)?);
        info!("Connected to {:?}", sockaddr);
        Ok(())
    }

    fn call_agent_once(&mut self, cmd: &Message) -> Result<Message, ErrType> {
        if self.stream.is_none() {
            self.connect()?;
        }
        let sock = self.stream.as_mut().unwrap();
        Self::write_message(sock, cmd)?;
        Self::read_message(sock)
    }

    fn call_agent(&mut self, cmd: &Message, retry: u32) -> Result<Message, ErrType> {
        let mut ret: Result<Message, ErrType> = Err(RsshErr::RetryLT1Err.into_ptr());
        for _i in 0..retry {
            ret = self.call_agent_once(cmd);
            if let Ok(val) = ret {
                return Ok(val);
            }
        }
        ret
    }

    pub fn list_identities(&mut self) -> Result<Vec<PublicKey>, ErrType> {
        let msg = self.call_agent(&Message::RequestIdentities, NET_RETRY_CNT)?;
        if let Message::IdentitiesAnswer(keys) = msg {
            let mut result = vec![];
            for item in keys {
                debug!("list_identities: {:02X?} ({})", item.pubkey_blob, item.comment);
                if let Ok(pubkey) = from_bytes(&item.pubkey_blob) {
                    result.push(pubkey);
                }
            }
            Ok(result)
        } else {
            Err(RsshErr::InvalidRspErr.into_ptr())
        }
    }

    fn decode_signature_blob(blob: &[u8], is_ecdsa: bool) -> Result<Vec<u8>, ErrType> {
        let sig: proto::Signature = from_bytes(&blob)?;
        if is_ecdsa {
            use openssl::ecdsa::EcdsaSig;
            use openssl::bn::BigNum;

            let data: proto::EcDsaSignatureData = from_bytes(&sig.blob)?;
            trace!("ECDSA signature: r={:02X?} s={:02X?}", data.r, data.s);
            let r = BigNum::from_slice(&data.r)?;
            let s = BigNum::from_slice(&data.s)?;

            Ok(EcdsaSig::from_private_components(r, s)?.to_der()?)
        } else {
            trace!("signature: blob={:02X?}", sig.blob);
            Ok(sig.blob)
        }
    }

    pub fn sign_data<'b>(
        &mut self,
        data: &'b [u8],
        pubkey: &'b PublicKey,
    ) -> Result<Vec<u8>, ErrType> {
        let mut flags = 0u32;
        let mut is_ecdsa = false;
        match pubkey {
            PublicKey::Rsa(_) => flags = signature::RSA_SHA2_256,
            PublicKey::EcDsa(_) => is_ecdsa = true,
            _ => {}
        }
        let args = proto::SignRequest {
            pubkey_blob: to_bytes(pubkey)?,
            data: data.to_vec(),
            flags,
        };
        let msg = self.call_agent(&Message::SignRequest(args), NET_RETRY_CNT)?;
        if let Message::Failure = msg {
            return Err(RsshErr::AgentFailureErr.into_ptr());
        }
        if let Message::SignResponse(val) = msg {
            // println!("signature payload: {:?}", val);
            Ok(Self::decode_signature_blob(&val, is_ecdsa)?)
        } else {
            Err(RsshErr::InvalidRspErr.into_ptr())
        }
    }
}

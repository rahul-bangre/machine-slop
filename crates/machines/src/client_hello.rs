use tokio::io::{AsyncRead, AsyncReadExt};

const HANDSHAKE_RECORD: u8 = 22;
const CLIENT_HELLO: u8 = 1;
const SERVER_NAME_EXTENSION: u16 = 0;
const HOST_NAME: u8 = 0;
const RECORD_HEADER_BYTES: usize = 5;
const MAX_RECORD_BYTES: usize = 1 << 14;
pub const MAX_CLIENT_HELLO_BYTES: usize = 1 << 16;
pub const ACCESS_DENIED_ALERT: [u8; 7] = [21, 3, 3, 0, 2, 2, 49];

#[derive(Debug, PartialEq, Eq)]
pub enum Parsed {
    Incomplete,
    Malformed,
    Complete { server_name: Option<String> },
}

pub struct ClientHello {
    pub bytes: Vec<u8>,
    pub server_name: Option<String>,
}

pub async fn read(
    guest: &mut (impl AsyncRead + Unpin),
    already_read: Vec<u8>,
) -> Option<ClientHello> {
    let mut bytes = already_read;
    loop {
        match parse(&bytes) {
            Parsed::Complete { server_name } => return Some(ClientHello { bytes, server_name }),
            Parsed::Malformed => return None,
            Parsed::Incomplete if bytes.len() >= MAX_CLIENT_HELLO_BYTES => return None,
            Parsed::Incomplete => {}
        }
        let mut chunk = [0; 4096];
        let room = (MAX_CLIENT_HELLO_BYTES - bytes.len()).min(chunk.len());
        match guest.read(&mut chunk[..room]).await {
            Ok(0) | Err(_) => return None,
            Ok(read) => bytes.extend_from_slice(&chunk[..read]),
        }
    }
}

pub fn parse(bytes: &[u8]) -> Parsed {
    let mut handshake = Vec::new();
    let mut records = bytes;
    loop {
        if let [message_type, a, b, c, body @ ..] = handshake.as_slice() {
            if *message_type != CLIENT_HELLO {
                return Parsed::Malformed;
            }
            let length = usize::from_be_bytes([0, 0, 0, 0, 0, *a, *b, *c]);
            if length > MAX_CLIENT_HELLO_BYTES {
                return Parsed::Malformed;
            }
            if body.len() >= length {
                return match server_name(&body[..length]) {
                    Some(server_name) => Parsed::Complete { server_name },
                    None => Parsed::Malformed,
                };
            }
        }
        let Some((header, rest)) = records.split_at_checked(RECORD_HEADER_BYTES) else {
            return Parsed::Incomplete;
        };
        let length = usize::from(u16::from_be_bytes([header[3], header[4]]));
        if header[0] != HANDSHAKE_RECORD
            || header[1] != 3
            || length == 0
            || length > MAX_RECORD_BYTES
        {
            return Parsed::Malformed;
        }
        let Some((fragment, rest)) = rest.split_at_checked(length) else {
            return Parsed::Incomplete;
        };
        handshake.extend_from_slice(fragment);
        records = rest;
    }
}

struct Reader<'a>(&'a [u8]);

impl<'a> Reader<'a> {
    fn take(&mut self, count: usize) -> Option<&'a [u8]> {
        let (taken, rest) = self.0.split_at_checked(count)?;
        self.0 = rest;
        Some(taken)
    }

    fn u8(&mut self) -> Option<u8> {
        Some(self.take(1)?[0])
    }

    fn u16(&mut self) -> Option<u16> {
        let bytes = self.take(2)?;
        Some(u16::from_be_bytes([bytes[0], bytes[1]]))
    }

    fn prefixed_u8(&mut self) -> Option<Reader<'a>> {
        let length = self.u8()?.into();
        self.take(length).map(Reader)
    }

    fn prefixed_u16(&mut self) -> Option<Reader<'a>> {
        let length = self.u16()?.into();
        self.take(length).map(Reader)
    }
}

fn server_name(body: &[u8]) -> Option<Option<String>> {
    let mut hello = Reader(body);
    hello.take(2 + 32)?;
    hello.prefixed_u8()?;
    hello.prefixed_u16()?;
    hello.prefixed_u8()?;
    if hello.0.is_empty() {
        return Some(None);
    }
    let mut extensions = hello.prefixed_u16()?;
    while !extensions.0.is_empty() {
        let extension_type = extensions.u16()?;
        let mut extension = extensions.prefixed_u16()?;
        if extension_type != SERVER_NAME_EXTENSION {
            continue;
        }
        let mut names = extension.prefixed_u16()?;
        while !names.0.is_empty() {
            let name_type = names.u8()?;
            let name = names.prefixed_u16()?.0;
            if name_type == HOST_NAME {
                return std::str::from_utf8(name)
                    .ok()
                    .filter(|name| name.is_ascii())
                    .map(|name| Some(name.to_ascii_lowercase()));
            }
        }
        return Some(None);
    }
    Some(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hello_body(server_name: Option<&str>) -> Vec<u8> {
        let mut body = vec![3, 3];
        body.extend([7; 32]);
        body.extend([0]);
        body.extend([0, 2, 0x13, 0x01]);
        body.extend([1, 0]);
        let mut extensions = Vec::new();
        extensions.extend([0, 43, 0, 3, 2, 3, 4]);
        if let Some(name) = server_name {
            let name = name.as_bytes();
            let list_length = 3 + name.len();
            extensions.extend(0u16.to_be_bytes());
            extensions.extend(u16::try_from(list_length + 2).unwrap().to_be_bytes());
            extensions.extend(u16::try_from(list_length).unwrap().to_be_bytes());
            extensions.push(HOST_NAME);
            extensions.extend(u16::try_from(name.len()).unwrap().to_be_bytes());
            extensions.extend(name);
        }
        body.extend(u16::try_from(extensions.len()).unwrap().to_be_bytes());
        body.extend(extensions);
        body
    }

    fn records(body: &[u8], fragment: usize) -> Vec<u8> {
        let mut handshake = vec![CLIENT_HELLO];
        handshake.extend(&u32::try_from(body.len()).unwrap().to_be_bytes()[1..]);
        handshake.extend(body);
        handshake
            .chunks(fragment)
            .flat_map(|chunk| {
                let mut record = vec![HANDSHAKE_RECORD, 3, 1];
                record.extend(u16::try_from(chunk.len()).unwrap().to_be_bytes());
                record.extend(chunk);
                record
            })
            .collect()
    }

    #[test]
    fn the_server_name_is_read_from_a_hello_split_across_records() {
        let hello = records(&hello_body(Some("Example.COM")), 7);
        assert_eq!(
            parse(&hello),
            Parsed::Complete {
                server_name: Some("example.com".into())
            }
        );
        for cut in [0, 3, hello.len() / 2, hello.len() - 1] {
            assert_eq!(parse(&hello[..cut]), Parsed::Incomplete, "cut at {cut}");
        }
    }

    #[test]
    fn a_hello_without_the_extension_names_no_server() {
        assert_eq!(
            parse(&records(&hello_body(None), 512)),
            Parsed::Complete { server_name: None }
        );
    }

    #[test]
    fn bytes_that_are_not_a_client_hello_are_malformed() {
        assert_eq!(parse(b"GET / HTTP/1.1\r\n\r\n"), Parsed::Malformed);
        let mut server_hello = records(&hello_body(Some("example.com")), 512);
        server_hello[5] = 2;
        assert_eq!(parse(&server_hello), Parsed::Malformed);
        let mut truncated_extensions = hello_body(Some("example.com"));
        truncated_extensions.truncate(truncated_extensions.len() - 3);
        assert_eq!(
            parse(&records(&truncated_extensions, 512)),
            Parsed::Malformed
        );
    }
}

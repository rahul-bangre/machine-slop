use std::net::IpAddr;

use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::UnixStream,
    time::timeout,
};

use crate::egress::{
    Host, MachineEgress, NoAddress, OPEN_TIMEOUT, REQUEST_HEAD_TIMEOUT, public_addresses,
};

const HEADER_BYTES: usize = 12;
const MAX_NAME_BYTES: usize = 255;
const MAX_ANSWERS: usize = 8;
const ANSWER_TTL_SECS: u32 = 30;
const TYPE_A: u16 = 1;
const TYPE_AAAA: u16 = 28;
const CLASS_IN: u16 = 1;
const RESPONSE: u16 = 0x8000;
const RECURSION_DESIRED: u16 = 0x0100;
const RECURSION_AVAILABLE: u16 = 0x0080;
const OPCODE: u16 = 0x7800;
const POINTER_TO_QUESTION: [u8; 2] = [0xc0, 0x0c];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u16)]
enum Rcode {
    NoError = 0,
    FormatError = 1,
    ServerFailure = 2,
    NotImplemented = 4,
    Refused = 5,
}

#[derive(Debug, PartialEq, Eq)]
struct Question<'a> {
    id: u16,
    recursion_desired: bool,
    name: String,
    record_type: u16,
    class: u16,
    wire: &'a [u8],
}

#[derive(Debug, PartialEq, Eq)]
enum Query<'a> {
    Question(Question<'a>),
    Unanswerable { id: u16, rcode: Rcode },
}

pub(crate) async fn serve_connection(mut guest: UnixStream, machine: &MachineEgress) {
    loop {
        let mut length = [0; 2];
        if !matches!(
            timeout(REQUEST_HEAD_TIMEOUT, guest.read_exact(&mut length)).await,
            Ok(Ok(_))
        ) {
            return;
        }
        let mut message = vec![0; usize::from(u16::from_be_bytes(length))];
        if !matches!(
            timeout(REQUEST_HEAD_TIMEOUT, guest.read_exact(&mut message)).await,
            Ok(Ok(_))
        ) {
            return;
        }
        let Some(response) = answer(&message, machine).await else {
            return;
        };
        let Ok(length) = u16::try_from(response.len()) else {
            return;
        };
        if guest.write_all(&length.to_be_bytes()).await.is_err()
            || guest.write_all(&response).await.is_err()
        {
            return;
        }
    }
}

async fn answer(message: &[u8], machine: &MachineEgress) -> Option<Vec<u8>> {
    let question = match parse(message)? {
        Query::Question(question) => question,
        Query::Unanswerable { id, rcode } => return Some(respond(id, false, None, rcode, &[])),
    };
    let respond_with = |rcode, addresses: &[IpAddr]| {
        respond(
            question.id,
            question.recursion_desired,
            Some(question.wire),
            rcode,
            addresses,
        )
    };
    let policy = machine.policy.borrow().clone();
    let Some(host @ Host::Name(_)) = Host::parse(&question.name) else {
        return Some(respond_with(Rcode::Refused, &[]));
    };
    if !policy.permits(&host) {
        return Some(respond_with(Rcode::Refused, &[]));
    }
    let wants_v4 = question.record_type == TYPE_A;
    if question.class != CLASS_IN || !(wants_v4 || question.record_type == TYPE_AAAA) {
        return Some(respond_with(Rcode::NoError, &[]));
    }
    let resolved = timeout(
        OPEN_TIMEOUT,
        public_addresses(&host, 0, policy.source(), &machine.resolutions),
    )
    .await;
    let addresses = match resolved {
        Ok(Ok(addresses)) => addresses,
        Ok(Err(NoAddress::Unresolvable)) | Err(_) => {
            return Some(respond_with(Rcode::ServerFailure, &[]));
        }
        Ok(Err(NoAddress::NotPublic | NoAddress::NoneFromSource(_))) => Vec::new(),
    };
    let mut matching: Vec<IpAddr> = addresses
        .into_iter()
        .map(|address| address.ip())
        .filter(|ip| ip.is_ipv4() == wants_v4)
        .collect();
    matching.dedup();
    matching.truncate(MAX_ANSWERS);
    Some(respond_with(Rcode::NoError, &matching))
}

fn parse(message: &[u8]) -> Option<Query<'_>> {
    let header = message.get(..HEADER_BYTES)?;
    let word = |at: usize| u16::from_be_bytes([header[at], header[at + 1]]);
    let (id, flags, questions) = (word(0), word(2), word(4));
    if flags & RESPONSE != 0 {
        return None;
    }
    if flags & OPCODE != 0 {
        let rcode = Rcode::NotImplemented;
        return Some(Query::Unanswerable { id, rcode });
    }
    let format_error = Query::Unanswerable {
        id,
        rcode: Rcode::FormatError,
    };
    if questions != 1 {
        return Some(format_error);
    }
    let mut labels = Vec::new();
    let mut at = HEADER_BYTES;
    loop {
        let Some(&length) = message.get(at) else {
            return Some(format_error);
        };
        at += 1;
        if length == 0 {
            break;
        }
        let label = match message.get(at..at + usize::from(length)) {
            Some(label) if length < 64 && label.is_ascii() => label,
            _ => return Some(format_error),
        };
        labels.push(String::from_utf8_lossy(label).to_ascii_lowercase());
        at += usize::from(length);
        if at - HEADER_BYTES > MAX_NAME_BYTES {
            return Some(format_error);
        }
    }
    let Some(&[type_high, type_low, class_high, class_low]) = message.get(at..at + 4) else {
        return Some(format_error);
    };
    Some(Query::Question(Question {
        id,
        recursion_desired: flags & RECURSION_DESIRED != 0,
        name: labels.join("."),
        record_type: u16::from_be_bytes([type_high, type_low]),
        class: u16::from_be_bytes([class_high, class_low]),
        wire: &message[HEADER_BYTES..at + 4],
    }))
}

fn respond(
    id: u16,
    recursion_desired: bool,
    question: Option<&[u8]>,
    rcode: Rcode,
    addresses: &[IpAddr],
) -> Vec<u8> {
    let recursion = if recursion_desired {
        RECURSION_DESIRED
    } else {
        0
    };
    let flags = RESPONSE | recursion | RECURSION_AVAILABLE | rcode as u16;
    let counts = [u16::from(question.is_some()), addresses.len() as u16, 0, 0];
    let mut response = Vec::with_capacity(512);
    response.extend(id.to_be_bytes());
    response.extend(flags.to_be_bytes());
    for count in counts {
        response.extend(count.to_be_bytes());
    }
    response.extend(question.unwrap_or_default());
    for address in addresses {
        let (record_type, data) = match address {
            IpAddr::V4(ip) => (TYPE_A, ip.octets().to_vec()),
            IpAddr::V6(ip) => (TYPE_AAAA, ip.octets().to_vec()),
        };
        response.extend(POINTER_TO_QUESTION);
        response.extend(record_type.to_be_bytes());
        response.extend(CLASS_IN.to_be_bytes());
        response.extend(ANSWER_TTL_SECS.to_be_bytes());
        response.extend((data.len() as u16).to_be_bytes());
        response.extend(data);
    }
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(id: u16, name: &str, record_type: u16) -> Vec<u8> {
        let mut message = id.to_be_bytes().to_vec();
        message.extend([0x01, 0x00, 0, 1, 0, 0, 0, 0, 0, 0]);
        for label in name.split('.') {
            message.push(label.len() as u8);
            message.extend(label.as_bytes());
        }
        message.push(0);
        message.extend(record_type.to_be_bytes());
        message.extend(CLASS_IN.to_be_bytes());
        message
    }

    #[test]
    fn a_question_is_parsed_case_insensitively_with_its_wire_form() {
        let message = query(7, "Example.COM", TYPE_AAAA);
        let Some(Query::Question(question)) = parse(&message) else {
            panic!("question expected");
        };
        assert_eq!(question.id, 7);
        assert!(question.recursion_desired);
        assert_eq!(question.name, "example.com");
        assert_eq!(question.record_type, TYPE_AAAA);
        assert_eq!(question.wire, &message[HEADER_BYTES..]);
    }

    #[test]
    fn responses_echo_the_question_and_point_answers_at_it() {
        let message = query(9, "example.com", TYPE_A);
        let response = respond(
            9,
            true,
            Some(&message[HEADER_BYTES..]),
            Rcode::NoError,
            &["93.184.215.14".parse().unwrap()],
        );
        assert_eq!(&response[..2], &9u16.to_be_bytes());
        assert_eq!(&response[2..4], &[0x81, 0x80]);
        assert_eq!(&response[4..12], &[0, 1, 0, 1, 0, 0, 0, 0]);
        assert_eq!(&response[12..message.len()], &message[HEADER_BYTES..]);
        assert_eq!(
            &response[message.len()..],
            &[0xc0, 0x0c, 0, 1, 0, 1, 0, 0, 0, 30, 0, 4, 93, 184, 215, 14]
        );
    }

    #[test]
    fn malformed_and_unsupported_queries_are_answered_without_a_lookup() {
        let mut two_questions = query(1, "example.com", TYPE_A);
        two_questions[5] = 2;
        assert_eq!(
            parse(&two_questions),
            Some(Query::Unanswerable {
                id: 1,
                rcode: Rcode::FormatError
            })
        );
        let mut compressed = query(2, "example.com", TYPE_A);
        compressed[HEADER_BYTES] = 0xc0;
        assert_eq!(
            parse(&compressed),
            Some(Query::Unanswerable {
                id: 2,
                rcode: Rcode::FormatError
            })
        );
        let mut status = query(3, "example.com", TYPE_A);
        status[2] |= 0x10;
        assert_eq!(
            parse(&status),
            Some(Query::Unanswerable {
                id: 3,
                rcode: Rcode::NotImplemented
            })
        );
        let mut response = query(4, "example.com", TYPE_A);
        response[2] |= 0x80;
        assert_eq!(parse(&response), None);
        assert_eq!(parse(&[0; 5]), None);
    }
}

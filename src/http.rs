// The tiny HTTP request reader shared by the stream server and the DLNA
// endpoints. Only what is needed to route a request: a start line, folded
// header names, and a length-bounded body

use std::io::BufRead;

pub struct Request {
    pub method: String,
    pub target: String,
    // Header names are lowercased, values keep their case
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

impl Request {
    // Reads one request off a buffered stream. Returns None on a malformed
    // start line or a truncated body, which the caller answers with 400
    pub fn parse<R: BufRead>(reader: &mut R) -> Option<Request> {
        let mut line = String::new();
        reader.read_line(&mut line).ok()?;
        let mut parts = line.split_whitespace();
        let method = parts.next()?.to_string();
        let target = parts.next()?.to_string();

        let mut headers = Vec::new();
        loop {
            let mut header = String::new();
            reader.read_line(&mut header).ok()?;
            let header = header.trim_end();
            if header.is_empty() {
                break;
            }
            if let Some((name, value)) = header.split_once(':') {
                headers.push((name.trim().to_ascii_lowercase(),
                              value.trim().to_string()));
            }
        }

        let length: usize = headers
            .iter()
            .find(|(name, _)| name == "content-length")
            .and_then(|(_, value)| value.parse().ok())
            .unwrap_or(0);
        let mut body = vec![0u8; length.min(65536)];
        if !body.is_empty() {
            reader.read_exact(&mut body).ok()?;
        }

        Some(Request { method, target, headers, body })
    }

    // The request target with any query string stripped
    pub fn path(&self) -> &str {
        self.target.split('?').next().unwrap_or("/")
    }

    pub fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Cursor;

    fn parse(raw: &str) -> Option<Request> {
        Request::parse(&mut Cursor::new(raw.as_bytes().to_vec()))
    }

    #[test]
    fn reads_method_and_strips_query_from_path() {
        let r = parse("GET /snapshot?foo=bar HTTP/1.1\r\nHost: x\r\n\r\n").unwrap();
        assert_eq!(r.method, "GET");
        assert_eq!(r.path(), "/snapshot");
    }

    #[test]
    fn folds_header_names_and_keeps_value_case() {
        let r = parse("GET / HTTP/1.1\r\nSOAPACTION: \"urn:Foo#Bar\"\r\n\r\n").unwrap();
        assert_eq!(r.header("soapaction"), Some("\"urn:Foo#Bar\""));
    }

    #[test]
    fn reads_body_up_to_content_length() {
        let r = parse("POST /dlna/cds/control HTTP/1.1\r\nContent-Length: 5\r\n\r\nhello extra")
            .unwrap();
        assert_eq!(r.body, b"hello");
    }

    #[test]
    fn rejects_an_empty_start_line() {
        assert!(parse("\r\n").is_none());
    }
}

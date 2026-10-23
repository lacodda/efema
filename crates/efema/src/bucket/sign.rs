//! Signing a request the way S3 checks it: AWS Signature Version 4.
//!
//! The whole of it is HMAC-SHA256 over a canonical spelling of the request,
//! with a key derived from the secret, the day, the region and the service.
//! A hundred lines rather than an SDK: efema makes five kinds of request to a
//! bucket, all of them signed the same way, and the signature is checked
//! against the examples in Amazon's own documentation below.

use std::fmt::Write;
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

/// The hash of an empty payload, which every request without a body signs.
pub(crate) const EMPTY_PAYLOAD: &str = "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855";

/// The service every request is signed for.
const SERVICE: &str = "s3";

/// What a bucket's owner hands out to reach it: an access key's id and its
/// secret, as S3 and every service that speaks its protocol issue them.
///
/// The secret never leaves the device: a request carries a signature made
/// with it, and nothing prints it - not `Debug`, not an error.
#[derive(Clone)]
pub struct Credentials {
    id: String,
    secret: String,
}

impl Credentials {
    /// The access key `id` with its `secret`.
    pub fn new(id: impl Into<String>, secret: impl Into<String>) -> Self {
        Self { id: id.into(), secret: secret.into() }
    }

    /// The access key's id.
    pub fn id(&self) -> &str {
        &self.id
    }
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Credentials({})", self.id)
    }
}

/// A moment as a signature names it: `20130524T000000Z`, and the day of it.
pub(crate) struct Stamp {
    pub(crate) time: String,
    date: String,
}

impl Stamp {
    pub(crate) fn at(moment: SystemTime) -> Self {
        let seconds = moment.duration_since(UNIX_EPOCH).map_or(0, |since| since.as_secs());
        let (year, month, day) = civil(seconds.div_euclid(86_400) as i64);
        let of_day = seconds % 86_400;
        let date = format!("{year:04}{month:02}{day:02}");
        let time = format!("{date}T{:02}{:02}{:02}Z", of_day / 3600, of_day % 3600 / 60, of_day % 60);
        Self { time, date }
    }
}

/// The calendar day of a count of days since 1970-01-01 (Howard Hinnant's
/// `civil_from_days`).
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let of_era = z.rem_euclid(146_097);
    let year_of_era = (of_era - of_era / 1460 + of_era / 36_524 - of_era / 146_096) / 365;
    let of_year = of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
    let shifted_month = (5 * of_year + 2) / 153;
    let day = (of_year - (153 * shifted_month + 2) / 5 + 1) as u32;
    let month = if shifted_month < 10 { shifted_month + 3 } else { shifted_month - 9 } as u32;
    let year = year_of_era + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// One request to sign. `path` and `query` are spelled as they are sent -
/// encoded with [`encode`] - and `headers` are every header the signature
/// covers, names in lowercase: at least `host`, `x-amz-date` and
/// `x-amz-content-sha256`.
pub(crate) struct Request<'a> {
    pub(crate) method: &'a str,
    pub(crate) path: &'a str,
    pub(crate) query: &'a str,
    pub(crate) headers: &'a [(String, String)],
    pub(crate) payload: &'a str,
}

/// The `Authorization` header that signs `request` at `stamp`.
pub(crate) fn authorization(credentials: &Credentials, region: &str, stamp: &Stamp, request: &Request<'_>) -> String {
    let mut headers: Vec<(&str, &str)> =
        request.headers.iter().map(|(name, value)| (name.as_str(), value.trim())).collect();
    headers.sort_unstable();
    let signed = headers.iter().map(|(name, _)| *name).collect::<Vec<_>>().join(";");
    let mut canonical = format!("{}\n{}\n{}\n", request.method, request.path, request.query);
    for (name, value) in &headers {
        let _ = writeln!(canonical, "{name}:{value}");
    }
    let _ = write!(canonical, "\n{signed}\n{}", request.payload);

    let scope = format!("{}/{region}/{SERVICE}/aws4_request", stamp.date);
    let to_sign = format!("AWS4-HMAC-SHA256\n{}\n{scope}\n{}", stamp.time, hex(&sha256(canonical.as_bytes())));
    let mut key = hmac(format!("AWS4{}", credentials.secret).as_bytes(), stamp.date.as_bytes());
    for part in [region, SERVICE, "aws4_request"] {
        key = hmac(&key, part.as_bytes());
    }
    let signature = hex(&hmac(&key, to_sign.as_bytes()));
    format!("AWS4-HMAC-SHA256 Credential={}/{scope}, SignedHeaders={signed}, Signature={signature}", credentials.id)
}

/// The hash of a payload, as `x-amz-content-sha256` carries it.
pub(crate) fn payload_hash(body: &[u8]) -> String {
    hex(&sha256(body))
}

fn sha256(data: &[u8]) -> [u8; 32] {
    Sha256::digest(data).into()
}

/// A text as S3 wants it in a path or a query: everything but letters,
/// digits and `-._~` percent-encoded - and `/` too, unless it is a path.
pub(crate) fn encode(text: &str, path: bool) -> String {
    let mut out = String::with_capacity(text.len());
    for byte in text.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'.' | b'_' | b'~') || (path && byte == b'/') {
            out.push(byte as char);
        } else {
            let _ = write!(out, "%{byte:02X}");
        }
    }
    out
}

/// A query in its canonical spelling, which is also the one sent: pairs
/// encoded, sorted by name and then by value.
pub(crate) fn query(pairs: &[(&str, &str)]) -> String {
    let mut encoded: Vec<(String, String)> =
        pairs.iter().map(|(name, value)| (encode(name, false), encode(value, false))).collect();
    encoded.sort_unstable();
    encoded.iter().map(|(name, value)| format!("{name}={value}")).collect::<Vec<_>>().join("&")
}

/// HMAC-SHA256 (RFC 2104): fifteen lines over the hash already in the tree.
fn hmac(key: &[u8], message: &[u8]) -> [u8; 32] {
    const BLOCK: usize = 64;
    let mut block = [0u8; BLOCK];
    if key.len() > BLOCK {
        block[..32].copy_from_slice(&sha256(key));
    } else {
        block[..key.len()].copy_from_slice(key);
    }
    let inner: [u8; 32] = Sha256::new().chain_update(block.map(|b| b ^ 0x36)).chain_update(message).finalize().into();
    Sha256::new().chain_update(block.map(|b| b ^ 0x5c)).chain_update(inner).finalize().into()
}

fn hex(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    // The examples of "Signature Calculations for the Authorization Header:
    // Transferring Payload in a Single Chunk" in the Amazon S3 API reference:
    // signatures computed by Amazon, not by this code.
    fn example() -> (Credentials, Stamp) {
        let credentials = Credentials::new("AKIAIOSFODNN7EXAMPLE", "wJalrXUtnFEMI/K7MDENG/bPxRfiCYEXAMPLEKEY");
        (credentials, Stamp::at(UNIX_EPOCH + Duration::from_secs(1_369_353_600)))
    }

    fn headers(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs.iter().map(|(name, value)| (name.to_string(), value.to_string())).collect()
    }

    fn signature(header: &str) -> &str {
        header.rsplit("Signature=").next().unwrap()
    }

    #[test]
    fn a_moment_is_spelled_as_the_signature_names_it() {
        let (_, stamp) = example();
        assert_eq!(stamp.time, "20130524T000000Z");
        assert_eq!(stamp.date, "20130524");
        let leap = Stamp::at(UNIX_EPOCH + Duration::from_secs(951_827_696));
        assert_eq!(leap.time, "20000229T123456Z");
        assert_eq!(Stamp::at(UNIX_EPOCH).time, "19700101T000000Z");
        assert_eq!(Stamp::at(UNIX_EPOCH + Duration::from_secs(4_102_444_799)).time, "20991231T235959Z");
    }

    #[test]
    fn signs_a_get_as_amazon_does() {
        let (credentials, stamp) = example();
        let header = authorization(
            &credentials,
            "us-east-1",
            &stamp,
            &Request {
                method: "GET",
                path: "/test.txt",
                query: "",
                headers: &headers(&[
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("range", "bytes=0-9"),
                    ("x-amz-content-sha256", EMPTY_PAYLOAD),
                    ("x-amz-date", "20130524T000000Z"),
                ]),
                payload: EMPTY_PAYLOAD,
            },
        );
        assert_eq!(
            header,
            "AWS4-HMAC-SHA256 Credential=AKIAIOSFODNN7EXAMPLE/20130524/us-east-1/s3/aws4_request, \
             SignedHeaders=host;range;x-amz-content-sha256;x-amz-date, \
             Signature=f0e8bdb87c964420e857bd35b5d6ed310bd44f0170aba48dd91039c6036bdb41"
        );
    }

    #[test]
    fn signs_a_put_as_amazon_does() {
        let (credentials, stamp) = example();
        let payload = payload_hash(b"Welcome to Amazon S3.");
        assert_eq!(payload, "44ce7dd67c959e0d3524ffac1771dfbba87d2b6b4b4e99e42034a8b803f8b072");
        let header = authorization(
            &credentials,
            "us-east-1",
            &stamp,
            &Request {
                method: "PUT",
                path: &format!("/{}", encode("test$file.text", true)),
                query: "",
                headers: &headers(&[
                    ("date", "Fri, 24 May 2013 00:00:00 GMT"),
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("x-amz-content-sha256", &payload),
                    ("x-amz-date", "20130524T000000Z"),
                    ("x-amz-storage-class", "REDUCED_REDUNDANCY"),
                ]),
                payload: &payload,
            },
        );
        assert_eq!(signature(&header), "98ad721746da40c64f1a55b78f14c238d841ea1380cd77a1b5971af0ece108bd");
    }

    #[test]
    fn signs_a_listing_as_amazon_does() {
        let (credentials, stamp) = example();
        let header = authorization(
            &credentials,
            "us-east-1",
            &stamp,
            &Request {
                method: "GET",
                path: "/",
                query: &query(&[("prefix", "J"), ("max-keys", "2")]),
                headers: &headers(&[
                    ("host", "examplebucket.s3.amazonaws.com"),
                    ("x-amz-content-sha256", EMPTY_PAYLOAD),
                    ("x-amz-date", "20130524T000000Z"),
                ]),
                payload: EMPTY_PAYLOAD,
            },
        );
        assert_eq!(signature(&header), "34b48302e7b5fa45bde8084f4b7868a86f0a534bc59db6670ed5711ef69dc6f7");
    }

    #[test]
    fn hmac_matches_rfc_4231() {
        // Test case 2: a short key; test case 6: a key longer than a block.
        assert_eq!(
            hex(&hmac(b"Jefe", b"what do ya want for nothing?")),
            "5bdcc146bf60754e6a042426089575c75a003f089d2739839dec58b964ec3843"
        );
        assert_eq!(
            hex(&hmac(&[0xaa; 131], b"Test Using Larger Than Block-Size Key - Hash Key First")),
            "60e431591ee0b67f0d8a26aacbf5b77f8e0bc6213728c5140546040f0ee37f54"
        );
    }

    #[test]
    fn a_query_is_encoded_and_sorted() {
        assert_eq!(
            query(&[("start-after", "notes/0a/b/9"), ("list-type", "2"), ("prefix", "a b")]),
            "list-type=2&prefix=a%20b&start-after=notes%2F0a%2Fb%2F9"
        );
        assert_eq!(encode("a/b c~", true), "a/b%20c~");
    }

    #[test]
    fn the_secret_is_never_printed() {
        let (credentials, _) = example();
        assert_eq!(format!("{credentials:?}"), "Credentials(AKIAIOSFODNN7EXAMPLE)");
    }
}

//! The little of S3's XML a client of a bucket reads: the keys of a listing,
//! whether it goes on, and the code and message of a refusal.
//!
//! Text between a tag's opening and closing, unescaped - not a general XML
//! reader. S3 answers in a fixed shape, without attributes on these tags,
//! without CDATA and without nesting a tag inside one of the same name, and
//! the keys efema writes are plain ASCII; anything that does not read this
//! way is not an answer of S3's.

/// The text of every `<tag>…</tag>` in `xml`, in order, unescaped; `None`
/// when one of them does not unescape.
pub(crate) fn all(xml: &str, tag: &str) -> Option<Vec<String>> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut found = Vec::new();
    let mut rest = xml;
    while let Some(start) = rest.find(&open) {
        let after = &rest[start + open.len()..];
        let end = after.find(&close)?;
        found.push(unescape(&after[..end])?);
        rest = &after[end + close.len()..];
    }
    Some(found)
}

/// The text of the first `<tag>…</tag>` in `xml`, unescaped.
pub(crate) fn first(xml: &str, tag: &str) -> Option<String> {
    all(xml, tag)?.into_iter().next()
}

/// The five entities XML predefines, and characters by number.
fn unescape(text: &str) -> Option<String> {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(at) = rest.find('&') {
        out.push_str(&rest[..at]);
        let entity = &rest[at + 1..];
        let end = entity.find(';')?;
        let name = &entity[..end];
        let character = match name {
            "amp" => '&',
            "lt" => '<',
            "gt" => '>',
            "quot" => '"',
            "apos" => '\'',
            _ => {
                let number = match name.strip_prefix("#x") {
                    Some(hex) => u32::from_str_radix(hex, 16).ok()?,
                    None => name.strip_prefix('#')?.parse().ok()?,
                };
                char::from_u32(number)?
            }
        };
        out.push(character);
        rest = &entity[end + 1..];
    }
    out.push_str(rest);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A listing as MinIO answers it, with its escaped ETag.
    const LISTING: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<ListBucketResult xmlns="http://s3.amazonaws.com/doc/2006-03-01/"><Name>probe</Name><Prefix>notes/b/</Prefix><KeyCount>2</KeyCount><MaxKeys>2</MaxKeys><IsTruncated>true</IsTruncated><Contents><Key>notes/b/99</Key><ETag>&#34;f97c5d29&#34;</ETag><Size>3</Size></Contents><Contents><Key>notes/b/98</Key><Size>4</Size></Contents><NextContinuationToken>1x&amp;y</NextContinuationToken></ListBucketResult>"#;

    #[test]
    fn reads_the_keys_of_a_listing_and_whether_it_goes_on() {
        assert_eq!(all(LISTING, "Key").unwrap(), ["notes/b/99", "notes/b/98"]);
        assert_eq!(first(LISTING, "IsTruncated").as_deref(), Some("true"));
        assert_eq!(first(LISTING, "NextContinuationToken").as_deref(), Some("1x&y"));
        assert_eq!(first(LISTING, "ETag").as_deref(), Some("\"f97c5d29\""));
        assert_eq!(first(LISTING, "Owner"), None);
    }

    #[test]
    fn reads_a_refusal() {
        let refusal = "<Error><Code>PreconditionFailed</Code><Message>At least one of the pre-conditions you specified did not hold</Message></Error>";
        assert_eq!(first(refusal, "Code").as_deref(), Some("PreconditionFailed"));
        assert!(first(refusal, "Message").unwrap().starts_with("At least one"));
    }

    #[test]
    fn unescapes_entities_and_refuses_what_is_not_one() {
        assert_eq!(
            unescape("a &lt;b&gt; &amp; &quot;c&quot; &apos;d&apos; &#65;&#x42;").unwrap(),
            "a <b> & \"c\" 'd' AB"
        );
        assert_eq!(unescape("&nbsp;"), None);
        assert_eq!(unescape("a & b"), None);
        assert_eq!(all("<Key>a &bogus</Key>", "Key"), None);
        assert_eq!(all("<Key>never closed", "Key"), None);
    }
}

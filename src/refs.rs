use percent_encoding::percent_decode_str;
use url::Url;

pub(crate) struct Target {
    pub(crate) document: Url,
    pub(crate) pointer: Vec<String>,
}

/// Resolve only local documents and JSON Pointer fragments. No network access.
pub(crate) fn resolve(base: &Url, reference: &str) -> Option<Target> {
    // URL parsing tolerates malformed percent escapes; reference syntax does not.
    let bytes = reference.as_bytes();
    for (index, &byte) in bytes.iter().enumerate() {
        if byte == b'%'
            && !bytes
                .get(index + 1..index + 3)
                .is_some_and(|escape| escape.iter().all(u8::is_ascii_hexdigit))
        {
            return None;
        }
    }
    let mut document = base.join(reference).ok()?;
    if document.scheme() != "file" || document.query().is_some() || document.to_file_path().is_err()
    {
        return None;
    }
    let fragment = percent_decode_str(document.fragment().unwrap_or(""))
        .decode_utf8()
        .ok()?;
    let pointer = if fragment.is_empty() {
        Vec::new()
    } else {
        fragment
            .strip_prefix('/')?
            .split('/')
            .map(|token| {
                let mut decoded = String::new();
                let mut chars = token.chars();
                while let Some(ch) = chars.next() {
                    decoded.push(if ch == '~' {
                        match chars.next()? {
                            '0' => '~',
                            '1' => '/',
                            _ => return None,
                        }
                    } else {
                        ch
                    });
                }
                Some(decoded)
            })
            .collect::<Option<Vec<_>>>()?
    };
    document.set_fragment(None);
    Some(Target { document, pointer })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_relative_uris_and_decodes_pointer_tokens_once() {
        let base = Url::parse("file:///specs/api/openapi.yaml").unwrap();
        let target = resolve(&base, "../models/pet%20types.json#/a~1b/~01/caf%C3%A9/0/").unwrap();
        assert_eq!(
            target.document.as_str(),
            "file:///specs/models/pet%20types.json"
        );
        assert_eq!(target.pointer, ["a/b", "~1", "café", "0", ""]);
        for reference in ["", "#", "../model.yaml"] {
            assert!(resolve(&base, reference).unwrap().pointer.is_empty());
        }
        for reference in [
            "https://example.com/spec.json",
            "#/bad~2",
            "#/bad~",
            "#/bad%GG",
            "#/%FF",
            "#anchor",
            "model.yaml?query",
        ] {
            assert!(resolve(&base, reference).is_none(), "{reference}");
        }
    }
}

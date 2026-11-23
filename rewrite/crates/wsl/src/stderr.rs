//! PowerShell's stderr, made into a short readable message.

/// Messages longer than this (in characters) are cut short.
const MAX_MESSAGE_CHARS: usize = 500;

/// Normally plain text; if PowerShell itself reported an error while stderr
/// wasn't a console, it is CLIXML, and the error strings are pulled out.
pub(crate) fn describe(stderr: &[u8]) -> String {
    let text = String::from_utf8_lossy(stderr);
    let message = if text.contains("#< CLIXML") {
        clixml_errors(&text)
    } else {
        text.trim().to_owned()
    };
    let message = message.split_whitespace().collect::<Vec<_>>().join(" ");
    if message.is_empty() {
        return "no error output".to_owned();
    }
    if message.chars().count() > MAX_MESSAGE_CHARS {
        format!(
            "{}...",
            message.chars().take(MAX_MESSAGE_CHARS).collect::<String>()
        )
    } else {
        message
    }
}

/// The `<S S="Error">` strings of a CLIXML stream, unescaped.
fn clixml_errors(text: &str) -> String {
    const OPEN: &str = "<S S=\"Error\">";
    let mut out = String::new();
    let mut rest = text;
    while let Some((_, after)) = rest.split_once(OPEN) {
        let (error, tail) = after.split_once("</S>").unwrap_or((after, ""));
        out.push_str(error);
        rest = tail;
    }
    out.replace("_x000D_", "")
        .replace("_x000A_", " ")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&amp;", "&")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_text_is_trimmed_and_empty_is_said_so() {
        assert_eq!(describe(b"  boom \r\n"), "boom");
        assert_eq!(describe(b""), "no error output");
    }

    #[test]
    fn clixml_errors_are_extracted_and_unescaped() {
        let clixml = concat!(
            "#< CLIXML\r\n<Objs Version=\"1.1.0.1\" xmlns=\"http://schemas.microsoft.com/powershell/2004/04\">",
            "<Obj S=\"progress\" RefId=\"0\"><TN RefId=\"0\"><T>System.Management.Automation.PSCustomObject</T></TN>",
            "<MS><PR N=\"Record\"><AV>Preparing modules for first use.</AV></PR></MS></Obj>",
            "<S S=\"Error\">Cannot invoke method. Method invocation is supported only on core types in this language mode._x000D__x000A_</S>",
            "<S S=\"Error\">At line:1 char:1 &amp; more_x000D__x000A_</S></Objs>"
        );
        assert_eq!(
            describe(clixml.as_bytes()),
            "Cannot invoke method. Method invocation is supported only on core types in \
             this language mode. At line:1 char:1 & more"
        );
        assert_eq!(
            describe(b"#< CLIXML\r\n<S S=\"Error\">unterminated"),
            "unterminated"
        );
    }

    #[test]
    fn long_messages_are_cut_short() {
        let long = "x".repeat(600);
        assert_eq!(describe(long.as_bytes()).len(), 503);
    }
}

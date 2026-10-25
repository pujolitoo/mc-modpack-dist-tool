//! Turns the game's log4j XML events into readable lines for the launcher log.
//!
//! The logging configuration Mojang ships with each Minecraft version (`client-1.12.xml`…) prints log4j events as
//! XML, so that a launcher can parse them (that file also carries the Log4Shell mitigation for old versions, so it
//! must not be removed). This converts
//!
//! ```text
//! <log4j:Event logger="…" timestamp="…" level="INFO" thread="Render thread">
//!   <log4j:Message><![CDATA[Hello]]></log4j:Message>
//! </log4j:Event>
//! ```
//!
//! into `[Render thread/INFO]: Hello` (the vanilla console format), with the stack trace (`log4j:Throwable`) on the
//! following lines. Lines that are not XML events (Forge, mods, `System.out`…) are passed through unchanged.

#[derive(Default)]
pub struct Parser {
    /// Text of the event being read (an event spans several lines).
    buf: Option<String>,
}

impl Parser {
    /// Feeds one output line. Returns the text to show, if any (it may contain several lines).
    pub fn feed(&mut self, line: &str) -> Option<String> {
        match &mut self.buf {
            Some(buf) => {
                buf.push_str(line);
                buf.push('\n');
                if line.contains("</log4j:Event>") {
                    let event = self.buf.take().unwrap_or_default();
                    return Some(format_event(&event).unwrap_or(event));
                }
                None
            }
            None => {
                let Some(start) = line.find("<log4j:Event") else {
                    // A closing tag without its event (output that started mid-event): nothing to show.
                    if line.trim() == "</log4j:Event>" {
                        return None;
                    }
                    return Some(line.to_string());
                };
                let before = line[..start].trim_end();
                let rest = &line[start..];
                if rest.contains("</log4j:Event>") {
                    let shown = format_event(rest).unwrap_or_else(|| rest.to_string());
                    return Some(if before.is_empty() { shown } else { format!("{before}\n{shown}") });
                }
                self.buf = Some(format!("{rest}\n"));
                if before.is_empty() { None } else { Some(before.to_string()) }
            }
        }
    }

    /// At the end of the output: whatever was left of an unfinished event, as is.
    pub fn finish(&mut self) -> Option<String> {
        self.buf.take().map(|b| b.trim_end().to_string()).filter(|b| !b.is_empty())
    }
}

/// Value of an XML attribute (`name="…"`) of the opening tag.
fn attr(tag: &str, name: &str) -> Option<String> {
    let key = format!(" {name}=\"");
    let start = tag.find(&key)? + key.len();
    let end = tag[start..].find('"')? + start;
    Some(unescape(&tag[start..end]))
}

/// Text inside `<tag>…</tag>`: CDATA sections (possibly several in a row) or escaped text.
fn element_text(event: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let start = event.find(&open)? + open.len();
    let end = event[start..].find(&close)? + start;
    let inner = &event[start..end];
    let mut out = String::new();
    let mut rest = inner;
    let mut had_cdata = false;
    while let Some(i) = rest.find("<![CDATA[") {
        had_cdata = true;
        out.push_str(&unescape(&rest[..i]));
        let body = &rest[i + 9..];
        let Some(j) = body.find("]]>") else {
            out.push_str(body);
            return Some(out);
        };
        out.push_str(&body[..j]);
        rest = &body[j + 3..];
    }
    if had_cdata {
        out.push_str(&unescape(rest).trim());
    } else {
        out.push_str(&unescape(rest));
    }
    Some(out)
}

fn unescape(s: &str) -> String {
    if !s.contains('&') {
        return s.to_string();
    }
    s.replace("&lt;", "<").replace("&gt;", ">").replace("&quot;", "\"").replace("&apos;", "'").replace("&#10;", "\n").replace("&amp;", "&")
}

/// `[thread/LEVEL]: message` (+ stack trace), or `None` if it does not look like a log4j event.
fn format_event(event: &str) -> Option<String> {
    let tag_end = event.find('>')?;
    let tag = &event[..tag_end];
    let level = attr(tag, "level").unwrap_or_else(|| "INFO".to_string());
    let thread = attr(tag, "thread").unwrap_or_default();
    let message = element_text(event, "log4j:Message").unwrap_or_default();
    let mut out = if thread.is_empty() { format!("[{level}]: {message}") } else { format!("[{thread}/{level}]: {message}") };
    if let Some(trace) = element_text(event, "log4j:Throwable") {
        let trace = trace.trim_end();
        if !trace.is_empty() {
            out.push('\n');
            out.push_str(trace);
        }
    }
    Some(out.trim_end().to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(input: &str) -> Vec<String> {
        let mut p = Parser::default();
        let mut out: Vec<String> = input.lines().filter_map(|l| p.feed(l)).collect();
        out.extend(p.finish());
        out
    }

    #[test]
    fn events_from_the_game() {
        let log = r#"<log4j:Event logger="net.minecraft.client.gui.components.ChatComponent" timestamp="1792890813698" level="INFO" thread="Render thread">
  <log4j:Message><![CDATA[[CHAT] <Pere> hola]]></log4j:Message>
</log4j:Event>

<log4j:Event logger="net.minecraft.client.server.IntegratedServer" timestamp="1792890830126" level="INFO" thread="Server thread">
  <log4j:Message><![CDATA[Saving and pausing game...]]></log4j:Message>
</log4j:Event>
[12:00:01] [main/INFO]: plain line from Forge"#;
        assert_eq!(
            run(log),
            vec![
                "[Render thread/INFO]: [CHAT] <Pere> hola",
                "",
                "[Server thread/INFO]: Saving and pausing game...",
                "[12:00:01] [main/INFO]: plain line from Forge",
            ]
        );
    }

    #[test]
    fn throwable_escaped_text_and_split_cdata() {
        let log = r#"<log4j:Event logger="x" timestamp="1" level="ERROR" thread="Worker-1">
<log4j:Message><![CDATA[Failed: ]]]]><![CDATA[> done]]></log4j:Message>
<log4j:Throwable><![CDATA[java.lang.RuntimeException: boom
	at a.b.C.d(C.java:1)
]]></log4j:Throwable>
</log4j:Event>
<log4j:Event logger="y" timestamp="2" level="WARN" thread="t &amp; u"><log4j:Message>a &lt;b&gt; c</log4j:Message></log4j:Event>"#;
        assert_eq!(
            run(log),
            vec![
                "[Worker-1/ERROR]: Failed: ]]> done\njava.lang.RuntimeException: boom\n\tat a.b.C.d(C.java:1)",
                "[t & u/WARN]: a <b> c",
            ]
        );
    }

    #[test]
    fn unfinished_event_is_kept() {
        assert_eq!(run("<log4j:Event level=\"INFO\" thread=\"main\">\n<log4j:Message>"), vec!["<log4j:Event level=\"INFO\" thread=\"main\">\n<log4j:Message>"]);
    }
}

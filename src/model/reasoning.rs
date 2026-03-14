#[derive(Debug, Clone, PartialEq, Eq)]
enum Segment {
    Text(String),
    Reasoning(String),
}

const TAG_NAMES: [&str; 4] = ["think", "thinking", "thought", "antthinking"];

pub fn strip_reasoning_tags(text: &str) -> String {
    let Some(segments) = split_reasoning_segments(text) else {
        return text.trim().to_string();
    };
    segments
        .into_iter()
        .filter_map(|segment| match segment {
            Segment::Text(value) => Some(value),
            Segment::Reasoning(_) => None,
        })
        .collect::<String>()
        .trim()
        .to_string()
}

pub fn split_inline_reasoning(text: &str) -> (Option<String>, String) {
    let Some(segments) = split_reasoning_segments(text) else {
        return (None, text.trim().to_string());
    };

    let mut reasoning = Vec::new();
    let mut content = String::new();
    for segment in segments {
        match segment {
            Segment::Text(value) => content.push_str(&value),
            Segment::Reasoning(value) => {
                if !value.trim().is_empty() {
                    reasoning.push(value.trim().to_string());
                }
            }
        }
    }

    let reasoning = (!reasoning.is_empty()).then(|| reasoning.join("\n"));
    (reasoning, content.trim().to_string())
}

fn split_reasoning_segments(text: &str) -> Option<Vec<Segment>> {
    let mut cursor = 0usize;
    let mut segments = Vec::new();
    let mut active_tag: Option<String> = None;
    let mut active_start = 0usize;

    while let Some(tag) = find_tag(text, cursor) {
        if active_tag.is_none() {
            if !tag.is_close {
                if tag.start > cursor {
                    segments.push(Segment::Text(text[cursor..tag.start].to_string()));
                }
                active_tag = Some(tag.name);
                active_start = tag.end;
            }
            cursor = tag.end;
            continue;
        }

        if tag.is_close && active_tag.as_deref() == Some(tag.name.as_str()) {
            let reasoning = text[active_start..tag.start].trim();
            if !reasoning.is_empty() {
                segments.push(Segment::Reasoning(reasoning.to_string()));
            }
            active_tag = None;
            cursor = tag.end;
            continue;
        }
        cursor = tag.end;
    }

    if active_tag.is_some() {
        let trailing = text[active_start..].trim();
        if !trailing.is_empty() {
            segments.push(Segment::Reasoning(trailing.to_string()));
        }
    } else if cursor < text.len() {
        segments.push(Segment::Text(text[cursor..].to_string()));
    }

    segments
        .iter()
        .any(|segment| matches!(segment, Segment::Reasoning(_)))
        .then_some(segments)
}

#[derive(Debug)]
struct TagMatch {
    start: usize,
    end: usize,
    is_close: bool,
    name: String,
}

fn find_tag(text: &str, from: usize) -> Option<TagMatch> {
    let remainder = &text[from..];
    let relative = remainder.find('<')?;
    let start = from + relative;
    let bytes = text.as_bytes();
    let mut index = start + 1;
    while index < text.len() && bytes[index].is_ascii_whitespace() {
        index += 1;
    }
    let mut is_close = false;
    if index < text.len() && bytes[index] == b'/' {
        is_close = true;
        index += 1;
        while index < text.len() && bytes[index].is_ascii_whitespace() {
            index += 1;
        }
    }

    let name_start = index;
    while index < text.len() && bytes[index].is_ascii_alphabetic() {
        index += 1;
    }
    if name_start == index {
        return find_tag(text, start + 1);
    }

    let name = text[name_start..index].to_ascii_lowercase();
    if !TAG_NAMES.iter().any(|candidate| *candidate == name) {
        return find_tag(text, start + 1);
    }

    let end_relative = text[index..].find('>')?;
    Some(TagMatch {
        start,
        end: index + end_relative + 1,
        is_close,
        name,
    })
}

#[cfg(test)]
mod tests {
    use super::{split_inline_reasoning, strip_reasoning_tags};

    #[test]
    fn splits_inline_think_block() {
        let (reasoning, content) =
            split_inline_reasoning("<think>internal</think>\n\nVisible answer");
        assert_eq!(reasoning.as_deref(), Some("internal"));
        assert_eq!(content, "Visible answer");
    }

    #[test]
    fn supports_tag_attributes() {
        let (reasoning, content) =
            split_inline_reasoning(r#"<think reason="careful">hidden</think>Shown"#);
        assert_eq!(reasoning.as_deref(), Some("hidden"));
        assert_eq!(content, "Shown");
    }

    #[test]
    fn strips_unclosed_reasoning_block() {
        assert_eq!(strip_reasoning_tags("<think>secret"), "");
    }

    #[test]
    fn strips_multiple_reasoning_blocks() {
        let cleaned =
            strip_reasoning_tags("Before<think>a</think>Middle<thinking>b</thinking>After");
        assert_eq!(cleaned, "BeforeMiddleAfter");
    }
}

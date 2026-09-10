pub(super) struct InterpolationFragment<'a> {
    pub(super) text: &'a str,
    pub(super) offset: usize,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum LexState {
    Normal,
    SingleQuoted,
    Backtick,
    LineComment,
    BlockComment,
}

pub(super) fn interpolation_fragments(
    text: &str,
) -> Result<Vec<InterpolationFragment<'_>>, String> {
    let body = interpolation_body(text)?;
    let bytes = body.as_bytes();
    let mut output = Vec::new();
    let body_offset = text.len().saturating_sub(body.len());
    let mut start = None;
    let mut depth = 0_usize;
    let mut state = LexState::Normal;
    let mut index = 0_usize;
    while index < bytes.len() {
        if depth == 0 {
            scan_outer_text(bytes, &mut index, &mut start, &mut depth)?;
            continue;
        }
        if state != LexState::Normal {
            scan_quoted_or_comment(bytes, &mut index, &mut state);
            continue;
        }
        match bytes[index] {
            b'\'' => state = LexState::SingleQuoted,
            b'`' => state = LexState::Backtick,
            b'/' if bytes.get(index + 1) == Some(&b'/') => {
                state = LexState::LineComment;
                index += 2;
                continue;
            }
            b'/' if bytes.get(index + 1) == Some(&b'*') => {
                state = LexState::BlockComment;
                index += 2;
                continue;
            }
            b'{' => depth += 1,
            b'}' => {
                depth -= 1;
                if depth == 0 {
                    push_fragment(body, body_offset, start.take(), index, &mut output)?;
                }
            }
            _ => {}
        }
        index += 1;
    }
    if depth != 0 {
        return Err("string interpolation contains an unclosed '{'".to_owned());
    }
    Ok(output)
}

fn interpolation_body(text: &str) -> Result<&str, String> {
    let body = text
        .strip_prefix('s')
        .or_else(|| text.strip_prefix('S'))
        .unwrap_or(text);
    body.strip_prefix('"')
        .and_then(|value| value.strip_suffix('"'))
        .ok_or_else(|| "string interpolation must use s\"...\" form".to_owned())
}

fn scan_outer_text(
    bytes: &[u8],
    index: &mut usize,
    start: &mut Option<usize>,
    depth: &mut usize,
) -> Result<(), String> {
    match bytes[*index] {
        b'\\' => *index = index.saturating_add(2),
        b'{' => {
            *start = Some(*index + 1);
            *depth = 1;
            *index += 1;
        }
        b'}' => return Err("string interpolation contains an unmatched '}'".to_owned()),
        _ => *index += 1,
    }
    Ok(())
}

fn scan_quoted_or_comment(bytes: &[u8], index: &mut usize, state: &mut LexState) {
    let byte = bytes[*index];
    match *state {
        LexState::SingleQuoted => {
            if byte == b'\\' {
                *index = index.saturating_add(2);
                return;
            }
            if byte == b'\'' && bytes.get(*index + 1) == Some(&b'\'') {
                *index += 2;
                return;
            }
            if byte == b'\'' {
                *state = LexState::Normal;
            }
        }
        LexState::Backtick => {
            if byte == b'`' && bytes.get(*index + 1) == Some(&b'`') {
                *index += 2;
                return;
            }
            if byte == b'`' {
                *state = LexState::Normal;
            }
        }
        LexState::LineComment if byte == b'\n' => *state = LexState::Normal,
        LexState::BlockComment if byte == b'*' && bytes.get(*index + 1) == Some(&b'/') => {
            *state = LexState::Normal;
            *index += 2;
            return;
        }
        LexState::Normal | LexState::LineComment | LexState::BlockComment => {}
    }
    *index += 1;
}

fn push_fragment<'a>(
    body: &'a str,
    body_offset: usize,
    start: Option<usize>,
    end: usize,
    output: &mut Vec<InterpolationFragment<'a>>,
) -> Result<(), String> {
    let begin = start.unwrap_or(end);
    let raw = &body[begin..end];
    let fragment = raw.trim();
    if fragment.is_empty() {
        return Err("string interpolation expression must not be empty".to_owned());
    }
    let leading = raw.len().saturating_sub(raw.trim_start().len());
    output.push(InterpolationFragment {
        text: fragment,
        offset: body_offset + begin + leading,
    });
    Ok(())
}

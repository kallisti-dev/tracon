//! Format strings for rows and status lines, in the style of starship.
//!
//! `{var}` inserts a variable, `{a|b}` the first of several with a value,
//! and `{var:<16}`, `{var:>4}` or `{var:^8}` pads it to a width, with
//! `{var:.20}` cutting it to a length. `[text](style)` styles what it
//! encloses, the style being words such as `bold dim italic underline
//! reversed`, a colour name, `bright-` and a name, `#rrggbb` or a number up
//! to 255, each optionally after `bg:`. `(…)` is a group shown only when a
//! variable inside it has a value. A backslash keeps the next character.
//!
//! Rendering gives styled spans; a front end draws them, or drops the
//! styles with [`plain`].

use crate::Error;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Template {
    source: String,
    items: Vec<Item>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum Item {
    Text(String),
    Variable(Variable),
    /// The style is itself a template, so `({style})` reads a variable.
    Styled {
        items: Vec<Item>,
        style: Vec<Item>,
    },
    Group(Vec<Item>),
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Variable {
    /// Alternatives, the first with a value being shown.
    names: Vec<String>,
    align: Align,
    width: Option<usize>,
    /// The most characters shown, the last one an ellipsis when cut.
    limit: Option<usize>,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum Align {
    #[default]
    Left,
    Right,
    Centre,
}

/// A run of text drawn in one style.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub text: String,
    pub style: Style,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Style {
    pub fg: Option<Colour>,
    pub bg: Option<Colour>,
    pub bold: bool,
    pub dim: bool,
    pub italic: bool,
    pub underline: bool,
    pub reversed: bool,
}

/// A terminal colour: one of the 256 indexed colours, the first sixteen
/// being the named ones, or a true colour.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Colour {
    Indexed(u8),
    Rgb(u8, u8, u8),
}

const NAMES: [&str; 8] = [
    "black", "red", "green", "yellow", "blue", "magenta", "cyan", "white",
];

impl Colour {
    fn parse(text: &str) -> Option<Colour> {
        if let Some(name) = text.strip_prefix("bright-") {
            return NAMES
                .iter()
                .position(|known| *known == name)
                .map(|index| Colour::Indexed(index as u8 + 8));
        }
        if let Some(index) = NAMES.iter().position(|known| *known == text) {
            return Some(Colour::Indexed(index as u8));
        }
        if let Some(hex) = text.strip_prefix('#')
            && hex.len() == 6
            && let Ok(value) = u32::from_str_radix(hex, 16)
        {
            let [_, r, g, b] = value.to_be_bytes();
            return Some(Colour::Rgb(r, g, b));
        }
        text.parse().ok().map(Colour::Indexed)
    }
}

impl Style {
    /// Parse words such as `bold yellow` or `bg:blue dim`; `none` drops what
    /// came before it.
    pub fn parse(text: &str) -> Result<Style, Error> {
        let mut style = Style::default();
        for word in text.split_whitespace() {
            match word {
                "bold" => style.bold = true,
                "dim" => style.dim = true,
                "italic" => style.italic = true,
                "underline" => style.underline = true,
                "reversed" => style.reversed = true,
                "none" => style = Style::default(),
                _ => {
                    let (background, colour) = match word.strip_prefix("bg:") {
                        Some(colour) => (true, colour),
                        None => (false, word.strip_prefix("fg:").unwrap_or(word)),
                    };
                    let colour = Colour::parse(colour).ok_or_else(|| Error::Invalid {
                        kind: "style",
                        value: word.to_owned(),
                    })?;
                    if background {
                        style.bg = Some(colour);
                    } else {
                        style.fg = Some(colour);
                    }
                }
            }
        }
        Ok(style)
    }

    /// This style applied over `outer`: colours it sets win, and attributes
    /// add up.
    fn over(&self, outer: &Style) -> Style {
        Style {
            fg: self.fg.or(outer.fg),
            bg: self.bg.or(outer.bg),
            bold: self.bold || outer.bold,
            dim: self.dim || outer.dim,
            italic: self.italic || outer.italic,
            underline: self.underline || outer.underline,
            reversed: self.reversed || outer.reversed,
        }
    }
}

fn malformed(reason: impl Into<String>) -> Error {
    Error::Template(reason.into())
}

impl Template {
    pub fn parse(source: &str) -> Result<Self, Error> {
        let chars: Vec<char> = source.chars().collect();
        let mut pos = 0;
        let items = parse_items(&chars, &mut pos, None)?;
        Ok(Template {
            source: source.to_owned(),
            items,
        })
    }

    pub fn source(&self) -> &str {
        &self.source
    }

    /// Every variable the template reads, styles included, in order of
    /// first appearance.
    pub fn variables(&self) -> Vec<&str> {
        let mut names = Vec::new();
        collect_variables(&self.items, &mut names);
        names
    }

    /// Render with `value` giving each variable's text; `None` and an empty
    /// string both mean no value.
    pub fn render(&self, value: &dyn Fn(&str) -> Option<String>) -> Vec<Span> {
        let mut spans = Vec::new();
        render_items(&self.items, value, &Style::default(), &mut spans);
        coalesce(spans)
    }
}

fn collect_variables<'a>(items: &'a [Item], names: &mut Vec<&'a str>) {
    for item in items {
        match item {
            Item::Text(_) => {}
            Item::Variable(variable) => {
                for name in &variable.names {
                    if !names.contains(&name.as_str()) {
                        names.push(name);
                    }
                }
            }
            Item::Styled { items, style } => {
                collect_variables(items, names);
                collect_variables(style, names);
            }
            Item::Group(items) => collect_variables(items, names),
        }
    }
}

fn parse_items(chars: &[char], pos: &mut usize, until: Option<char>) -> Result<Vec<Item>, Error> {
    let mut items = Vec::new();
    let mut text = String::new();
    let flush = |text: &mut String, items: &mut Vec<Item>| {
        if !text.is_empty() {
            items.push(Item::Text(std::mem::take(text)));
        }
    };
    while let Some(&c) = chars.get(*pos) {
        if Some(c) == until {
            break;
        }
        *pos += 1;
        match c {
            '\\' => {
                let next = chars
                    .get(*pos)
                    .ok_or_else(|| malformed("a backslash at the end"))?;
                text.push(*next);
                *pos += 1;
            }
            '{' => {
                flush(&mut text, &mut items);
                items.push(Item::Variable(parse_variable(chars, pos)?));
            }
            '[' => {
                flush(&mut text, &mut items);
                let inner = parse_items(chars, pos, Some(']'))?;
                expect(chars, pos, ']', "an unclosed [")?;
                expect(chars, pos, '(', "a style in parentheses must follow ]")?;
                let style = parse_items(chars, pos, Some(')'))?;
                expect(chars, pos, ')', "an unclosed ( after ]")?;
                items.push(Item::Styled {
                    items: inner,
                    style,
                });
            }
            '(' => {
                flush(&mut text, &mut items);
                let inner = parse_items(chars, pos, Some(')'))?;
                expect(chars, pos, ')', "an unclosed (")?;
                items.push(Item::Group(inner));
            }
            ']' | ')' | '}' => return Err(malformed(format!("an unmatched {c}"))),
            _ => text.push(c),
        }
    }
    flush(&mut text, &mut items);
    Ok(items)
}

fn expect(chars: &[char], pos: &mut usize, wanted: char, otherwise: &str) -> Result<(), Error> {
    if chars.get(*pos) == Some(&wanted) {
        *pos += 1;
        Ok(())
    } else {
        Err(malformed(otherwise))
    }
}

/// The inside of `{…}`, after the opening brace.
fn parse_variable(chars: &[char], pos: &mut usize) -> Result<Variable, Error> {
    let start = *pos;
    while chars.get(*pos).is_some_and(|c| *c != '}') {
        *pos += 1;
    }
    expect(chars, pos, '}', "an unclosed {")?;
    let inside: String = chars[start..*pos - 1].iter().collect();
    let (names, spec) = inside
        .split_once(':')
        .map_or((inside.as_str(), ""), |(names, spec)| (names, spec));
    let names: Vec<String> = names
        .split('|')
        .map(|name| name.trim().to_owned())
        .collect();
    let well_formed = |name: &str| {
        !name.is_empty()
            && name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
    };
    if !names.iter().all(|name| well_formed(name)) {
        return Err(malformed(format!("a bad variable name in {{{inside}}}")));
    }
    let mut variable = Variable {
        names,
        align: Align::Left,
        width: None,
        limit: None,
    };
    let mut spec = spec.trim();
    if let Some(rest) = spec.strip_prefix(['<', '>', '^']) {
        variable.align = match spec.chars().next() {
            Some('>') => Align::Right,
            Some('^') => Align::Centre,
            _ => Align::Left,
        };
        spec = rest;
    }
    let (width, limit) = spec.split_once('.').unwrap_or((spec, ""));
    let number = |text: &str| -> Result<Option<usize>, Error> {
        if text.is_empty() {
            return Ok(None);
        }
        text.parse()
            .map(Some)
            .map_err(|_| malformed(format!("a bad width in {{{inside}}}")))
    };
    variable.width = number(width)?;
    variable.limit = if spec.contains('.') {
        Some(number(limit)?.ok_or_else(|| malformed(format!("a bad width in {{{inside}}}")))?)
    } else {
        None
    };
    Ok(variable)
}

/// Render `items` in `style` onto `spans`, returning how many variables
/// were read and how many had a value.
fn render_items(
    items: &[Item],
    value: &dyn Fn(&str) -> Option<String>,
    style: &Style,
    spans: &mut Vec<Span>,
) -> (usize, usize) {
    let mut read = 0;
    let mut filled = 0;
    for item in items {
        match item {
            Item::Text(text) => spans.push(Span {
                text: text.clone(),
                style: style.clone(),
            }),
            Item::Variable(variable) => {
                read += 1;
                let found = variable
                    .names
                    .iter()
                    .find_map(|name| value(name).filter(|text| !text.is_empty()));
                if found.is_some() {
                    filled += 1;
                }
                let text = fit(&found.unwrap_or_default(), variable);
                if !text.is_empty() {
                    spans.push(Span {
                        text,
                        style: style.clone(),
                    });
                }
            }
            Item::Styled {
                items,
                style: words,
            } => {
                let mut rendered = Vec::new();
                render_items(words, value, &Style::default(), &mut rendered);
                let inner = Style::parse(&plain(&rendered)).unwrap_or_default();
                let (inner_read, inner_filled) =
                    render_items(items, value, &inner.over(style), spans);
                read += inner_read;
                filled += inner_filled;
            }
            Item::Group(items) => {
                let mut rendered = Vec::new();
                let (inner_read, inner_filled) = render_items(items, value, style, &mut rendered);
                read += inner_read;
                filled += inner_filled;
                if inner_read == 0 || inner_filled > 0 {
                    spans.extend(rendered);
                }
            }
        }
    }
    (read, filled)
}

/// `text` cut to the variable's limit and padded to its width.
fn fit(text: &str, variable: &Variable) -> String {
    let mut text = text.to_owned();
    if let Some(limit) = variable.limit
        && text.chars().count() > limit
    {
        text = text.chars().take(limit.saturating_sub(1)).collect();
        text.push('…');
    }
    if let Some(width) = variable.width {
        let length = text.chars().count();
        if length < width {
            let padding = width - length;
            let (left, right) = match variable.align {
                Align::Left => (0, padding),
                Align::Right => (padding, 0),
                Align::Centre => (padding / 2, padding - padding / 2),
            };
            text = format!("{}{text}{}", " ".repeat(left), " ".repeat(right));
        }
    }
    text
}

/// Join neighbouring spans of one style.
fn coalesce(spans: Vec<Span>) -> Vec<Span> {
    let mut joined: Vec<Span> = Vec::new();
    for span in spans {
        match joined.last_mut() {
            Some(last) if last.style == span.style => last.text.push_str(&span.text),
            _ => joined.push(span),
        }
    }
    joined
}

/// The text of `spans`, without their styles.
pub fn plain(spans: &[Span]) -> String {
    spans.iter().map(|span| span.text.as_str()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn render(template: &str, values: &[(&str, &str)]) -> Vec<Span> {
        Template::parse(template).unwrap().render(&|name| {
            values
                .iter()
                .find(|(known, _)| *known == name)
                .map(|(_, value)| (*value).to_owned())
        })
    }

    fn text(template: &str, values: &[(&str, &str)]) -> String {
        plain(&render(template, values))
    }

    #[test]
    fn variables_are_padded_cut_and_given_alternatives() {
        let values = [("label", "done"), ("age", "5m"), ("title", "a title")];
        assert_eq!(
            text("{label:<6}|{age:>4}|{age:^4}", &values),
            "done  |  5m| 5m "
        );
        assert_eq!(
            text("{label:.3}|{label:.10}|{label:6.3}", &values),
            "do…|done|do…   "
        );
        assert_eq!(text("{message|title}", &values), "a title");
        assert_eq!(text("{missing}|{missing:<3}|", &values), "|   |");
        assert_eq!(text(r"\{label\} \\ \(x\)", &values), r"{label} \ (x)");
    }

    #[test]
    fn groups_vanish_when_their_variables_are_empty() {
        assert_eq!(
            text(
                "{label}( · {background} in background)",
                &[("label", "done")]
            ),
            "done"
        );
        assert_eq!(
            text(
                "{label}( · {background} in background)",
                &[("label", "done"), ("background", "2")]
            ),
            "done · 2 in background"
        );
        // Text alone stays, and a nested group counts for its parent.
        assert_eq!(text("(a)(b({x}))", &[]), "a");
        assert_eq!(text("(b({x}))", &[("x", "1")]), "b1");
    }

    #[test]
    fn styles_nest_and_read_variables() {
        let spans = render(
            "[{icon} [{label}](red)]({style}) plain",
            &[("icon", "✓"), ("label", "done"), ("style", "bold green")],
        );
        let bold_green = Style {
            fg: Some(Colour::Indexed(2)),
            bold: true,
            ..Style::default()
        };
        let bold_red = Style {
            fg: Some(Colour::Indexed(1)),
            ..bold_green.clone()
        };
        assert_eq!(
            spans,
            [
                Span {
                    text: "✓ ".into(),
                    style: bold_green
                },
                Span {
                    text: "done".into(),
                    style: bold_red
                },
                Span {
                    text: " plain".into(),
                    style: Style::default()
                },
            ]
        );
        // An unknown style leaves the text unstyled rather than failing.
        assert_eq!(render("[x](blod)", &[])[0].style, Style::default());
    }

    #[test]
    fn styles_parse_colours_and_attributes() {
        let style = Style::parse("bold bg:bright-blue #ff8800 dim").unwrap();
        assert_eq!(style.fg, Some(Colour::Rgb(255, 136, 0)));
        assert_eq!(style.bg, Some(Colour::Indexed(12)));
        assert!(style.bold && style.dim && !style.italic);
        assert_eq!(Style::parse("208").unwrap().fg, Some(Colour::Indexed(208)));
        assert_eq!(Style::parse("red none").unwrap(), Style::default());
        assert_eq!(
            Style::parse("blod").unwrap_err().to_string(),
            "invalid style \"blod\""
        );
        assert!(Style::parse("256").is_err());
    }

    #[test]
    fn malformed_templates_are_rejected() {
        for (template, reason) in [
            ("{label", "an unclosed {"),
            ("[x", "an unclosed ["),
            ("[x]", "a style in parentheses must follow ]"),
            ("[x](bold", "an unclosed ( after ]"),
            ("(x", "an unclosed ("),
            ("x)", "an unmatched )"),
            ("x}", "an unmatched }"),
            ("{Label}", "a bad variable name in {Label}"),
            ("{}", "a bad variable name in {}"),
            ("{label:<x}", "a bad width in {label:<x}"),
            ("{label:.}", "a bad width in {label:.}"),
            (r"x\", "a backslash at the end"),
        ] {
            let error = Template::parse(template).unwrap_err().to_string();
            assert_eq!(error, reason, "{template}");
        }
    }

    #[test]
    fn variables_are_listed_once_each() {
        let template = Template::parse("{a}[{b|a}]({style}) ({c:<3})").unwrap();
        assert_eq!(template.variables(), ["a", "b", "style", "c"]);
        assert_eq!(template.source(), "{a}[{b|a}]({style}) ({c:<3})");
    }
}

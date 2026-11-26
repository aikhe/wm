//! Remembers how each kind of window was last kept, how wide its column was
//! and which named workspace it was on, so the next one of that kind opens
//! the same way, in this run or a later one.
//!
//! The rules live in `%APPDATA%\wm\rules.txt`, one per line, with tabs
//! between the mode, the program's file name, the window class and, when set,
//! the column width in pixels, `full`, and `@` before the workspace's name. A
//! kind that opens in its default mode at the default width on the workspace
//! on screen needs no rule.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::{env, fs};

use crate::layout::{Mode, Width};

/// A kind of window: its program's file name, then its window class.
type Kind = (String, String);

/// How windows of one kind open.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Rule {
    pub mode: Mode,
    /// The width of the window's column whenever it's tiled, or `None` for
    /// the default.
    pub width: Option<Width>,
    /// The named workspace the kind was last on, where it opens while none of
    /// its windows are open. `None` for the workspace on screen.
    pub workspace: Option<String>,
}

impl Rule {
    /// The rule of a kind without one, whose default mode is `mode`.
    pub fn opening(mode: Mode) -> Self {
        Rule {
            mode,
            width: None,
            workspace: None,
        }
    }
}

pub const MODES: [(Mode, &str); 4] = [
    (Mode::Tiled, "tiled"),
    (Mode::Floating, "float"),
    (Mode::Sticky, "sticky"),
    (Mode::OnTop, "top"),
];

pub struct Rules {
    /// `None` when there is nowhere to keep the file. The rules then only
    /// last for this run.
    path: Option<PathBuf>,
    rules: BTreeMap<Kind, Rule>,
    /// Whether the rules have changed since the file was read.
    unsaved: bool,
}

impl Rules {
    /// Reads the rules file. A missing or unreadable one means no rules.
    pub fn load() -> Self {
        let path =
            env::var_os("APPDATA").map(|folder| PathBuf::from(folder).join("wm").join("rules.txt"));
        let text = path.as_ref().and_then(|path| fs::read_to_string(path).ok());
        Rules {
            path,
            rules: parse(&text.unwrap_or_default()),
            unsaved: false,
        }
    }

    #[cfg(test)]
    pub fn empty() -> Self {
        Rules {
            path: None,
            rules: BTreeMap::new(),
            unsaved: false,
        }
    }

    /// The rule for windows of this kind: its default mode, `opening`, at the
    /// default width unless something else was set.
    pub fn get_by(&self, exe: Option<&str>, class: &str, opening: Mode) -> Rule {
        let rule = exe.and_then(|exe| {
            self.rules
                .get(&(exe.to_owned(), class.to_owned()))
                .cloned()
        });
        rule.unwrap_or(Rule::opening(opening))
    }

    /// Sets the rule for windows of this kind, whose default mode is `opening`.
    pub fn set_by(&mut self, exe: Option<String>, class: String, rule: Rule, opening: Mode) {
        let Some(exe) = exe else {
            return;
        };
        let kind = (exe, class);
        let default = Rule::opening(opening);
        let old = if rule == default {
            self.rules.remove(&kind)
        } else {
            self.rules.insert(kind, rule.clone())
        };
        self.unsaved |= old.unwrap_or(default) != rule;
    }

    /// Writes the rules to the file if they have changed.
    pub fn save(&mut self) {
        let (true, Some(path)) = (self.unsaved, &self.path) else {
            return;
        };
        let folder = path.parent().map_or(Ok(()), fs::create_dir_all);
        match folder.and_then(|()| fs::write(path, render(&self.rules))) {
            Ok(()) => self.unsaved = false,
            Err(error) => eprintln!("couldn't save {}: {error}", path.display()),
        }
    }
}

/// Lines that aren't a rule are skipped.
fn parse(text: &str) -> BTreeMap<Kind, Rule> {
    let rule = |line: &str| {
        let mut fields = line.split('\t');
        let (word, exe, class) = (fields.next()?, fields.next()?, fields.next()?);
        let (mode, _) = MODES.into_iter().find(|(_, known)| *known == word)?;
        let (mut pixels, mut full, mut workspace) = (None, false, None);
        for field in fields {
            match field.strip_prefix('@') {
                Some(name) => workspace = Some(name.to_owned()),
                None if field == "full" => full = true,
                None => pixels = field.parse().ok().or(pixels),
            }
        }
        let rule = Rule {
            mode,
            width: pixels.map(|pixels| Width { pixels, full }),
            workspace,
        };
        Some(((exe.to_owned(), class.to_owned()), rule))
    };
    text.lines().filter_map(rule).collect()
}

fn render(rules: &BTreeMap<Kind, Rule>) -> String {
    let line = |((exe, class), rule): (&Kind, &Rule)| {
        let (_, word) = MODES.into_iter().find(|(mode, _)| *mode == rule.mode)?;
        let width = match rule.width {
            None => String::new(),
            Some(Width { pixels, full: true }) => format!("\t{pixels}\tfull"),
            Some(Width { pixels, .. }) => format!("\t{pixels}"),
        };
        let workspace = match &rule.workspace {
            Some(name) => format!("\t@{name}"),
            None => String::new(),
        };
        Some(format!("{word}\t{exe}\t{class}{width}{workspace}\n"))
    };
    rules.iter().filter_map(line).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rules_survive_being_saved_and_read_back() {
        let text = "tiled\tDiscord.exe\tChrome_WidgetWin_1\t@chat\n\
                    top\tmpv.exe\tmpv\n\
                    float\tscrcpy.exe\tSDL_app\t640\n\
                    tiled\twezterm-gui.exe\torg.wezfurlong.wezterm\t1264\tfull\t@code\n";
        assert_eq!(render(&parse(text)), text);
    }

    #[test]
    fn lines_that_are_not_rules_are_skipped() {
        let rules = parse("# mine\nwide\ta.exe\tA\nfloat\tb.exe\n\nsticky\tc.exe\tC\n");
        let kept: Vec<_> = rules.into_iter().collect();
        let sticky = Rule::opening(Mode::Sticky);
        assert_eq!(kept, [(("c.exe".into(), "C".into()), sticky)]);
    }
}

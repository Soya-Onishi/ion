//! Contains the binary logic of Ion.
mod completer;
mod designators;
mod history;
mod prompt;
mod readln;

use self::{prompt::prompt, readln::readln};
use ion_shell::{
    builtins::man_pages,
    parser::{Expander, Terminator},
    status::Status,
    Shell,
};
use itertools::Itertools;
use liner::{Buffer, Context, KeyBindings};
use std::{cell::RefCell, path::Path, rc::Rc};

pub const MAN_ION: &str = "NAME
    Ion - The Ion shell

SYNOPSIS
    ion [options] [args...]

DESCRIPTION
    Ion is a commandline shell created to be a faster and easier to use alternative to the
    currently available shells. It is not POSIX compliant.

OPTIONS:
    -c <command>        evaluates given commands instead of reading from the commandline.

    -n or --no-execute
        do not execute any commands, just do syntax checking.

    -v or --version
        prints the version, platform and revision of ion then exits.

ARGS:
    <args>...    Script arguments (@args). If the -c option is not specified, the first
                 parameter is taken as a filename to execute";

pub(crate) const MAN_HISTORY: &str = r#"NAME
    history - print command history

SYNOPSIS
    history

DESCRIPTION
    Prints the command history."#;

pub struct InteractiveBinary<'a> {
    context: Rc<RefCell<Context>>,
    shell:   RefCell<Shell<'a>>,
}

impl<'a> InteractiveBinary<'a> {
    pub fn new(shell: Shell<'a>) -> Self {
        let mut context = Context::new();
        context.word_divider_fn = Box::new(word_divide);
        if shell.variables().get_str("HISTFILE_ENABLED") == Some("1".into()) {
            let path = shell.variables().get_str("HISTFILE").expect("shell didn't set HISTFILE");
            if !Path::new(path.as_str()).exists() {
                eprintln!("ion: creating history file at \"{}\"", path);
            }
            let _ = context.history.set_file_name_and_load_history(path.as_str());
        }
        InteractiveBinary { context: Rc::new(RefCell::new(context)), shell: RefCell::new(shell) }
    }

    /// Handles commands given by the REPL, and saves them to history.
    pub fn save_command(&self, cmd: &str) {
        if !cmd.ends_with('/')
            && self.shell.borrow().tilde(cmd).map_or(false, |path| Path::new(&path).is_dir())
        {
            self.save_command_in_history(&[cmd, "/"].concat());
        } else {
            self.save_command_in_history(cmd);
        }
    }

    pub fn add_callbacks(&self) {
        let mut shell = self.shell.borrow_mut();
        let context = self.context.clone();
        shell.set_prep_for_exit(Some(Box::new(move |shell| {
            // context will be sent a signal to commit all changes to the history file,
            // and waiting for the history thread in the background to finish.
            if shell.opts().huponexit {
                shell.resume_stopped();
                shell.background_send(sys::SIGHUP);
            }
            context.borrow_mut().history.commit_to_file();
        })));

        let context = self.context.clone();
        shell.set_on_command(Some(Box::new(move |shell, elapsed| {
            // If `RECORD_SUMMARY` is set to "1" (True, Yes), then write a summary of the
            // pipline just executed to the the file and context histories. At the
            // moment, this means record how long it took.
            if Some("1".into()) == shell.variables().get_str("RECORD_SUMMARY") {
                let summary = format!(
                    "#summary# elapsed real time: {}.{:09} seconds",
                    elapsed.as_secs(),
                    elapsed.subsec_nanos()
                );
                println!("{:?}", summary);
                context.borrow_mut().history.push(summary.into()).unwrap_or_else(|err| {
                    eprintln!("ion: history append: {}", err);
                });
            }
        })));
    }

    /// Creates an interactive session that reads from a prompt provided by
    /// Liner.
    pub fn execute_interactive(self) -> ! {
        let context_bis = self.context.clone();
        let history = &move |args: &[small::String], _shell: &mut Shell| -> Status {
            if man_pages::check_help(args, MAN_HISTORY) {
                return Status::SUCCESS;
            }

            print!("{}", context_bis.borrow().history.buffers.iter().format("\n"));
            Status::SUCCESS
        };

        let context_bis = self.context.clone();
        let keybindings = &move |args: &[small::String], _shell: &mut Shell| -> Status {
            match args.get(1).map(|s| s.as_str()) {
                Some("vi") => {
                    context_bis.borrow_mut().key_bindings = KeyBindings::Vi;
                    Status::SUCCESS
                }
                Some("emacs") => {
                    context_bis.borrow_mut().key_bindings = KeyBindings::Emacs;
                    Status::SUCCESS
                }
                Some(_) => Status::error("Invalid keybindings. Choices are vi and emacs"),
                None => Status::error("keybindings need an argument"),
            }
        };

        // change the lifetime to allow adding local builtins
        let InteractiveBinary { context, shell } = self;
        let this = InteractiveBinary { context, shell: RefCell::new(shell.into_inner()) };

        this.shell.borrow_mut().builtins_mut().add(
            "history",
            history,
            "Display a log of all commands previously executed",
        );
        this.shell.borrow_mut().builtins_mut().add(
            "keybindings",
            keybindings,
            "Change the keybindings",
        );
        this.shell.borrow_mut().evaluate_init_file();

        loop {
            let mut lines = std::iter::repeat_with(|| this.readln())
                .filter_map(|cmd| cmd)
                .flat_map(|s| s.into_bytes().into_iter().chain(Some(b'\n')));
            match Terminator::new(&mut lines).terminate() {
                Some(command) => {
                    this.shell.borrow_mut().unterminated = false;
                    let cmd: &str = &designators::expand_designators(
                        &this.context.borrow(),
                        command.trim_end(),
                    );
                    this.shell.borrow_mut().on_command(&cmd);
                    this.save_command(&cmd);
                }
                None => {
                    this.shell.borrow_mut().unterminated = true;
                }
            }
        }
    }

    /// Set the keybindings of the underlying liner context
    #[inline]
    pub fn set_keybindings(&mut self, key_bindings: KeyBindings) {
        self.context.borrow_mut().key_bindings = key_bindings;
    }

    /// Ion's interface to Liner's `read_line` method, which handles everything related to
    /// rendering, controlling, and getting input from the prompt.
    #[inline]
    pub fn readln(&self) -> Option<String> { readln(self) }

    /// Generates the prompt that will be used by Liner.
    #[inline]
    pub fn prompt(&self) -> String { prompt(&self.shell.borrow_mut()) }
}

#[derive(Debug)]
struct WordDivide<I>
where
    I: Iterator<Item = (usize, char)>,
{
    iter:       I,
    count:      usize,
    word_start: Option<usize>,
}
impl<I> WordDivide<I>
where
    I: Iterator<Item = (usize, char)>,
{
    #[inline]
    fn check_boundary(&mut self, c: char, index: usize, escaped: bool) -> Option<(usize, usize)> {
        if let Some(start) = self.word_start {
            if c == ' ' && !escaped {
                self.word_start = None;
                Some((start, index))
            } else {
                self.next()
            }
        } else {
            if c != ' ' {
                self.word_start = Some(index);
            }
            self.next()
        }
    }
}
impl<I> Iterator for WordDivide<I>
where
    I: Iterator<Item = (usize, char)>,
{
    type Item = (usize, usize);

    fn next(&mut self) -> Option<Self::Item> {
        self.count += 1;
        match self.iter.next() {
            Some((i, '\\')) => {
                if let Some((_, cnext)) = self.iter.next() {
                    self.count += 1;
                    // We use `i` in order to include the backslash as part of the word
                    self.check_boundary(cnext, i, true)
                } else {
                    self.next()
                }
            }
            Some((i, c)) => self.check_boundary(c, i, false),
            None => {
                // When start has been set, that means we have encountered a full word.
                self.word_start.take().map(|start| (start, self.count - 1))
            }
        }
    }
}

fn word_divide(buf: &Buffer) -> Vec<(usize, usize)> {
    // -> impl Iterator<Item = (usize, usize)> + 'a
    WordDivide { iter: buf.chars().cloned().enumerate(), count: 0, word_start: None }.collect() // TODO: return iterator directly :D
}

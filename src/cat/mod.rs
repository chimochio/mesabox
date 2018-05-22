//TODO: find dates of commits by other authors

//
// Copyright (c) 2018, The MesaLock Linux Project Contributors
// All rights reserved.
// 
// This work is licensed under the terms of the BSD 3-Clause License.
// For a copy, see the LICENSE file.
//
// This file incorporates work covered by the following copyright and
// permission notice:
//
//     Copyright (c) 2013-2018, Jordi Boggiano
//     Copyright (c) 2013-2018, Alex Lyon
//     Copyright (c) Evgeniy Klyuchikov
//     Copyright (c) Joshua S. Miller
//
//     Permission is hereby granted, free of charge, to any person obtaining a
//     copy of this software and associated documentation files (the
//     "Software"), to deal in the Software without restriction, including
//     without limitation the rights to use, copy, modify, merge, publish,
//     distribute, sublicense, and/or sell copies of the Software, and to
//     permit persons to whom the Software is furnished to do so, subject to
//     the following conditions:
//
//     The above copyright notice and this permission notice shall be included
//     in all copies or substantial portions of the Software.
//
//     THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS
//     OR IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF
//     MERCHANTABILITY, FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT.
//     IN NO EVENT SHALL THE AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY
//     CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER IN AN ACTION OF CONTRACT,
//     TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN CONNECTION WITH THE
//     SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
//

use clap::Arg;
use quick_error::ResultExt;
use std::ffi::{OsString, OsStr};
use std::fs::{metadata, File};
use std::iter;
use std::io::{self, BufWriter, Read, Write};
use super::{UtilSetup, /*ArgsIter, */UtilRead, UtilWrite, is_tty};

/// Unix domain socket support
#[cfg(unix)]
use std::net::Shutdown;
#[cfg(unix)]
use std::os::unix::fs::FileTypeExt;
#[cfg(unix)]
use std::os::unix::net::UnixStream;

pub const NAME: &str = "cat";
pub const DESCRIPTION: &str = "Concatenate FILE(s), or standard input, to standard output
With no FILE, or when FILE is -, read standard input.";

#[derive(PartialEq)]
enum NumberingMode {
    NumberNone,
    NumberNonEmpty,
    NumberAll,
}

// TODO: convert to use failure
quick_error! {
    #[derive(Debug)]
    enum CatError {
        /// Wrapper for io::Error with path context
        Input(err: io::Error, path: String) {
            display("{0}: {1}", path, err)
            context(path: &'a str, err: io::Error) -> (err, path.to_owned())
            cause(err)
        }

        /// Wrapper for io::Error with no context
        Output(err: io::Error) {
            display("{0}", err) from()
            cause(err)
        }

        /// Uknown Filetype  classification
        UnknownFiletype(path: String) {
            display("{0}: unknown filetype", path)
        }

        /// At least one error was encountered in reading or writing
        EncounteredErrors(count: usize) {
            display("encountered {0} error(s)", count)
        }

        /// Denotes an error caused by trying to `cat` a directory
        IsDirectory(path: String) {
            display("{0}: Is a directory", path)
        }
    }
}

struct OutputOptions<'a> {
    /// Line numbering mode
    number: NumberingMode,

    /// Suppress repeated empty output lines
    squeeze_blank: bool,

    /// display newlines as `end_of_line`
    show_ends: bool,

    /// display TAB characters as `tab`
    show_tabs: bool,

    /// If `show_tabs == true`, this string will be printed in the
    /// place of tabs
    tab: &'a str,

    /// Can be set to show characters other than '\n' a the end of
    /// each line, e.g. $
    end_of_line: &'a str,

    /// use ^ and M- notation, except for LF (\\n) and TAB (\\t)
    show_nonprint: bool,
}

impl<'a> OutputOptions<'a> {
    fn can_write_fast(&self) -> bool {
        !(self.show_tabs || self.show_nonprint || self.show_ends || self.squeeze_blank
            || self.number != NumberingMode::NumberNone)
    }
}

impl<'a> Default for OutputOptions<'a> {
    fn default() -> Self {
        Self {
            number: NumberingMode::NumberNone,
            squeeze_blank: false,
            show_ends: false,
            show_tabs: false,
            tab: "",
            end_of_line: "",
            show_nonprint: false,
        }
    }
}

/// Represents an open file handle, stream, or other device
struct InputHandle<R: Read> {
    reader: R,
    is_interactive: bool,
}

/// Concrete enum of recognized file types.
///
/// *Note*: `cat`-ing a directory should result in an
/// CatError::IsDirectory
enum InputType {
    Directory,
    File,
    StdIn,
    SymLink,
    #[cfg(unix)]
    BlockDevice,
    #[cfg(unix)]
    CharacterDevice,
    #[cfg(unix)]
    Fifo,
    #[cfg(unix)]
    Socket,
}

/// State that persists between output of each file
struct OutputState {
    /// The current line number
    line_number: usize,

    /// Whether the output cursor is at the beginning of a new line
    at_line_start: bool,
}

// XXX: is ArgsIter even needed?  i think the traits it needs might satisfy the reqs anyway
struct Cat<'a, I, O, E>
where
    I: for<'b> UtilRead<'b> + 'a,
    O: for<'b> UtilWrite<'b> + 'a,
    E: for<'b> UtilWrite<'b> + 'a,
{
    setup: &'a mut UtilSetup<I, O, E>,
}

impl<'a, I, O, E> Cat<'a, I, O, E>
where
    I: for<'b> UtilRead<'b>,
    O: for<'b> UtilWrite<'b>,
    E: for<'b> UtilWrite<'b>,
{
    pub fn new(setup: &'a mut UtilSetup<I, O, E>) -> Self {
        Self {
            setup: setup,
        }
    }

    /// Writes files to stdout with no configuration.  This allows a
    /// simple memory copy. Returns `Ok(())` if no errors were
    /// encountered, or an error with the number of errors encountered.
    ///
    /// # Arguments
    ///
    /// * `files` - There is no short circuit when encountiner an error
    /// reading a file in this vector
    pub fn write_fast<'b, T>(&mut self, files: T) -> CatResult<()>
    where
        T: Iterator<Item = &'b OsStr>,
    {
        let writer = &mut self.setup.stdout;
        let mut in_buf = [0; 1024 * 64];
        let mut error_count = 0;

        for file in files {
            let res = Self::open_and_exec(&file, &mut self.setup.stdin, |handle| {
                while let Ok(n) = handle.reader.read(&mut in_buf) {
                    if n == 0 {
                        break;
                    }
                    writer.write_all(&in_buf[..n]).context(&file.to_string_lossy()[..])?;
                }
                Ok(())
            });
            if let Err(error) = res {
                display_msg!(&mut self.setup.stderr, "{}", error)?;
                error_count += 1;
            }
        }

        if error_count == 0 {
            Ok(())
        } else {
            Err(CatError::EncounteredErrors(error_count))
        }
    }

    /// Writes files to stdout with `options` as configuration.  Returns
    /// `Ok(())` if no errors were encountered, or an error with the
    /// number of errors encountered.
    ///
    /// # Arguments
    ///
    /// * `files` - There is no short circuit when encountiner an error
    /// reading a file in this vector
    pub fn write_lines<'b, 'c, T>(&mut self, files: T, options: &OutputOptions<'c>) -> CatResult<()>
    where
        T: Iterator<Item = &'b OsStr>,
    {
        let mut error_count = 0;
        let mut state = OutputState {
            line_number: 1,
            at_line_start: true,
        };

        for file in files {
            if let Err(error) = self.write_file_lines(&file, options, &mut state) {
                display_msg!(&mut self.setup.stderr, "{}", error).context(&file.to_string_lossy()[..])?;
                error_count += 1;
            }
        }

        match error_count {
            0 => Ok(()),
            _ => Err(CatError::EncounteredErrors(error_count)),
        }
    }

    /// Outputs file contents to stdout in a linewise fashion,
    /// propagating any errors that might occur.
    fn write_file_lines<'b>(&mut self, file: &OsStr, options: &OutputOptions<'b>, state: &mut OutputState) -> CatResult<()> {
        //let mut handle = self.open(file)?;
        let mut in_buf = [0; 1024 * 31];
        // TODO: this is a waste of an allocation if the file is invalid, so move to write_lines and pass as an argument
        // TODO: maybe pass the callback as well so it can become an FnMut and be reused?
        let mut writer = BufWriter::with_capacity(1024 * 64, &mut self.setup.stdout);
        Self::open_and_exec(file, &mut self.setup.stdin, |handle| {
            let mut one_blank_kept = false;

            while let Ok(n) = handle.reader.read(&mut in_buf) {
                if n == 0 {
                    break;
                }
                let in_buf = &in_buf[..n];
                let mut pos = 0;
                while pos < n {
                    // skip empty line_number enumerating them if needed
                    if in_buf[pos] == '\n' as u8 {
                        if !state.at_line_start || !options.squeeze_blank || !one_blank_kept {
                            one_blank_kept = true;
                            if state.at_line_start && options.number == NumberingMode::NumberAll {
                                write!(&mut writer, "{0:6}\t", state.line_number)?;
                                state.line_number += 1;
                            }
                            writer.write_all(options.end_of_line.as_bytes())?;
                            if handle.is_interactive {
                                writer.flush().context(&file.to_string_lossy()[..])?;
                            }
                        }
                        state.at_line_start = true;
                        pos += 1;
                        continue;
                    }
                    one_blank_kept = false;
                    if state.at_line_start && options.number != NumberingMode::NumberNone {
                        write!(&mut writer, "{0:6}\t", state.line_number)?;
                        state.line_number += 1;
                    }

                    // print to end of line or end of buffer
                    let offset = if options.show_nonprint {
                        write_nonprint_to_end(&in_buf[pos..], &mut writer, options.tab.as_bytes())
                    } else if options.show_tabs {
                        write_tab_to_end(&in_buf[pos..], &mut writer)
                    } else {
                        write_to_end(&in_buf[pos..], &mut writer)
                    }?;
                    // end of buffer?
                    if offset == 0 {
                        state.at_line_start = false;
                        break;
                    }
                    // print suitable end of line
                    writer.write_all(options.end_of_line.as_bytes())?;
                    if handle.is_interactive {
                        writer.flush()?;
                    }
                    state.at_line_start = true;
                    pos += offset;
                }
            }

            Ok(())
        })
    }

    /// Returns an InputHandle from which a Reader can be accessed or an
    /// error
    ///
    /// # Arguments
    ///
    /// * `path` - `InputHandler` will wrap a reader from this file path
    fn open_and_exec<T>(path: &OsStr, stdin: &mut I, func: T) -> CatResult<()>
    where
        T: FnOnce(InputHandle<&mut Read>) -> CatResult<()>,
    {
        if path == "-" {
            let interactive = is_tty(stdin);
            return func(InputHandle {
                reader: stdin,
                is_interactive: interactive,
            });
        }

        let lossy_path = path.to_string_lossy();
        match get_input_type(path)? {
            InputType::Directory => Err(CatError::IsDirectory(lossy_path.into_owned())),
            #[cfg(unix)]
            InputType::Socket => {
                let mut socket = UnixStream::connect(path).context(&lossy_path[..])?;
                socket.shutdown(Shutdown::Write).context(&lossy_path[..])?;
                func(InputHandle {
                    reader: &mut socket as &mut Read,
                    is_interactive: false,
                })
            }
            _ => {
                let mut file = File::open(path).context(&lossy_path[..])?;
                func(InputHandle {
                    reader: &mut file as &mut Read,
                    is_interactive: false,
                })
            }
        }
    }
}

type CatResult<T> = Result<T, CatError>;

pub fn execute<I, O, E, T, U>(setup: &mut UtilSetup<I, O, E>, args: T) -> super::Result<()>
where
    I: for<'a> UtilRead<'a>,
    O: for<'a> UtilWrite<'a>,
    E: for<'a> UtilWrite<'a>,
    T: Iterator<Item = U>,
    U: Into<OsString> + Clone,
{
    let mut app = util_app!("cat")
                    .arg(Arg::with_name("show-all")
                            .short("A")
                            .long("show-all")
                            .help("equivalent to -vET"))
                    .arg(Arg::with_name("number-nonblank")
                            .short("b")
                            .long("number-nonblank")
                            .overrides_with("number")
                            .help("number nonempty output lines, overrides -n"))
                    .arg(Arg::with_name("e")
                            .short("e")
                            .help("equivalent to -vE"))
                    .arg(Arg::with_name("show-ends")
                            .short("E")
                            .long("show-ends")
                            .help("display $ at the end of each line"))
                    .arg(Arg::with_name("number")
                            .short("n")
                            .long("number")
                            .help("number all output lines"))
                    .arg(Arg::with_name("squeeze-blank")
                            .short("s")
                            .long("squeeze-blank")
                            .help("suppress repeated empty output lines"))
                    .arg(Arg::with_name("t")
                            .short("t")
                            .help("equivalent to -vT"))
                    .arg(Arg::with_name("show-tabs")
                            .short("T")
                            .long("show-tabs")
                            .help("display TAB characters as ^I"))
                    .arg(Arg::with_name("show-nonprinting")
                            .short("v")
                            .long("show-nonprinting")
                            .help("use ^ and M- notation, except for LF (\\n) and TAB (\\t)"))
                    .arg(Arg::with_name("FILES")
                            .index(1)
                            .multiple(true));
    let matches = get_matches!(setup, app, args);

    let mut options = OutputOptions::default();

    options.number = if matches.is_present("number-nonblank") {
        NumberingMode::NumberNonEmpty
    } else if matches.is_present("number") {
        NumberingMode::NumberAll
    } else {
        NumberingMode::NumberNone
    };

    options.show_nonprint = matches.is_present("show-all") || matches.is_present("show-nonprinting") || matches.is_present("e") || matches.is_present("t");
    options.show_ends = matches.is_present("show-all") || matches.is_present("show-ends") || matches.is_present("e");
    options.show_tabs = matches.is_present("show-all") || matches.is_present("show-tabs") || matches.is_present("t");
    options.squeeze_blank = matches.is_present("squeeze-blank");

    if let Some(files) = matches.values_of_os("FILES") {
        run(setup, files, options)
    } else {
        run(setup, iter::once(OsStr::new("-")), options)
    }
}

fn run<'a, 'b, I, O, E, T>(setup: &mut UtilSetup<I, O, E>, files: T, mut options: OutputOptions<'b>) -> super::Result<()>
where
    I: for<'c> UtilRead<'c>,
    O: for<'c> UtilWrite<'c>,
    E: for<'c> UtilWrite<'c>,
    T: Iterator<Item = &'a OsStr>,
{
    let can_write_fast = options.can_write_fast();
    
    let mut util = Cat::new(setup);

    if can_write_fast {
        util.write_fast(files)?;
    } else {
        options.tab = if options.show_tabs { "^I" } else { "\t" };
        options.end_of_line = if options.show_ends { "$\n" } else { "\n" };

        util.write_lines(files, &options)?;
    };

    Ok(())
}

/// Classifies the `InputType` of file at `path` if possible
///
/// # Arguments
///
/// * `path` - Path on a file system to classify metadata
fn get_input_type(path: &OsStr) -> CatResult<InputType> {
    if path == "-" {
        return Ok(InputType::StdIn);
    }

    let lossy_path = path.to_string_lossy();
    match metadata(path).context(&lossy_path[..])?.file_type() {
        #[cfg(unix)]
        ft if ft.is_block_device() =>
        {
            Ok(InputType::BlockDevice)
        }
        #[cfg(unix)]
        ft if ft.is_char_device() =>
        {
            Ok(InputType::CharacterDevice)
        }
        #[cfg(unix)]
        ft if ft.is_fifo() =>
        {
            Ok(InputType::Fifo)
        }
        #[cfg(unix)]
        ft if ft.is_socket() =>
        {
            Ok(InputType::Socket)
        }
        ft if ft.is_dir() => Ok(InputType::Directory),
        ft if ft.is_file() => Ok(InputType::File),
        ft if ft.is_symlink() => Ok(InputType::SymLink),
        _ => Err(CatError::UnknownFiletype(lossy_path.into_owned())),
    }
}

// write***_to_end methods
// Write all symbols till end of line or end of buffer is reached
// Return the (number of written symbols + 1) or 0 if the end of buffer is reached
fn write_to_end<W: Write>(in_buf: &[u8], writer: &mut W) -> CatResult<usize> {
    Ok(match in_buf.iter().position(|c| *c == '\n' as u8) {
        Some(p) => {
            writer.write_all(&in_buf[..p])?;
            p + 1
        }
        None => {
            writer.write_all(in_buf)?;
            0
        }
    })
}

fn write_tab_to_end<W: Write>(mut in_buf: &[u8], writer: &mut W) -> CatResult<usize> {
    loop {
        match in_buf
            .iter()
            .position(|c| *c == '\n' as u8 || *c == '\t' as u8)
        {
            Some(p) => {
                writer.write_all(&in_buf[..p])?;
                if in_buf[p] == '\n' as u8 {
                    return Ok(p + 1);
                } else {
                    writer.write_all("^I".as_bytes())?;
                    in_buf = &in_buf[p + 1..];
                }
            }
            None => {
                writer.write_all(in_buf)?;
                return Ok(0);
            }
        };
    }
}

fn write_nonprint_to_end<W: Write>(in_buf: &[u8], writer: &mut W, tab: &[u8]) -> CatResult<usize> {
    let mut count = 0;

    for byte in in_buf.iter().map(|c| *c) {
        if byte == b'\n' {
            break;
        }
        match byte {
            9 => writer.write_all(tab),
            0...8 | 10...31 => writer.write_all(&['^' as u8, byte + 64]),
            32...126 => writer.write_all(&[byte]),
            127 => writer.write_all(&['^' as u8, byte - 64]),
            128...159 => writer.write_all(&['M' as u8, '-' as u8, '^' as u8, byte - 64]),
            160...254 => writer.write_all(&['M' as u8, '-' as u8, byte - 128]),
            _ => writer.write_all(&['M' as u8, '-' as u8, '^' as u8, 63]),
        }?;
        count += 1;
    }
    if count != in_buf.len() {
        Ok(count + 1)
    } else {
        Ok(0)
    }
}

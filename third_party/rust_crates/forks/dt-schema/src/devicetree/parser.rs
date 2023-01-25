// Copyright 2022 The Fuchsia Authors. All rights reserved
// Use of this source code is governed by a BSD-style
// license that can be found in the LICENSE file.

use super::{property::Property, Node};
use byteorder::{BigEndian, ReadBytesExt};
use std::{
    ffi::CStr,
    io::{BufRead, BufReader, Read, Seek, SeekFrom},
    rc::Rc,
};

const DTB_MAGIC: u32 = 0xd00dfeed;
const DTB_MIN_VERSION: u32 = 16;

#[derive(Debug)]
pub enum Token {
    BeginNode(String),
    EndNode,
    Prop(Property),
    Nop,
    End,
}

pub struct Parser<R> {
    reader: BufReader<R>,
    // TODO(simonshields): validate against this.
    #[allow(unused)]
    struct_end: u32,

    strings: Vec<u8>,
}

#[derive(thiserror::Error, Debug)]
pub enum ParseError {
    #[error("Invalid magic")]
    InvalidMagic,
    #[error("Version is not supported: {0}")]
    UnsupportedVersion(u32),
    #[error("Invalid string offset: {0}")]
    InvalidStringOffset(usize),
    #[error("String not null-terminated: {0}")]
    StringNotTerminated(usize),
    #[error("Invalid string")]
    InvalidString(#[from] std::ffi::FromBytesWithNulError),
    #[error("Invalid string encoding")]
    InvalidStringEncoding(#[from] std::str::Utf8Error),
    #[error("Invalid token at offset {0:x}: {1:x}")]
    InvalidToken(u64, u32),
    #[error("Unexpected token")]
    UnexpectedToken(Token),
    #[error("I/O error")]
    IoError(#[from] std::io::Error),
}

impl<R: Read + Seek> Parser<R> {
    pub fn new(reader: R) -> Result<Self, ParseError> {
        let mut reader = BufReader::new(reader);
        let magic = reader.read_u32::<BigEndian>()?;
        if magic != DTB_MAGIC {
            return Err(ParseError::InvalidMagic);
        }

        let _ = reader.read_u32::<BigEndian>()?; // totalsize

        let off_dt_struct = reader.read_u32::<BigEndian>()?;
        let off_dt_string = reader.read_u32::<BigEndian>()?;
        let _ = reader.read_u32::<BigEndian>()?; // off_mem_rsvmap
        let version = reader.read_u32::<BigEndian>()?;
        let last_comp_version = reader.read_u32::<BigEndian>()?;
        if version < DTB_MIN_VERSION || DTB_MIN_VERSION < last_comp_version {
            return Err(ParseError::UnsupportedVersion(version));
        }

        let _ = reader.read_u32::<BigEndian>()?; // boot_cpuid_phys

        let size_dt_string = reader.read_u32::<BigEndian>()?;
        let size_dt_struct = reader.read_u32::<BigEndian>()?;

        reader.seek(SeekFrom::Start(off_dt_string.into()))?;
        let mut strings_vec: Vec<u8> = vec![0; size_dt_string.try_into().unwrap()];

        reader.read_exact(strings_vec.as_mut_slice())?;

        reader.seek(SeekFrom::Start(off_dt_struct.into()))?;

        Ok(Parser {
            reader,
            struct_end: off_dt_struct + size_dt_struct,
            strings: strings_vec,
        })
    }

    fn offset(&mut self) -> Result<u64, std::io::Error> {
        self.reader.seek(SeekFrom::Current(0))
    }

    fn string_at(&self, offset: usize) -> Result<String, ParseError> {
        if offset > self.strings.len() {
            return Err(ParseError::InvalidStringOffset(offset));
        }

        let slice = &self.strings[offset..];
        let (byte_index, _) = slice
            .iter()
            .enumerate()
            .find(|(_, &v)| v == 0)
            .ok_or(ParseError::StringNotTerminated(offset))?;

        let slice = &self.strings[offset..byte_index + offset + 1];
        let c_string = CStr::from_bytes_with_nul(slice)?;

        Ok(c_string.to_str()?.to_owned())
    }

    /// Align the parser to the next 4 byte boundary.
    fn align(&mut self) -> Result<(), ParseError> {
        let offset = self.offset()? as usize;
        if offset % std::mem::size_of::<u32>() != 0 {
            let adjust = std::mem::size_of::<u32>() - (offset % std::mem::size_of::<u32>());
            self.reader
                .seek(SeekFrom::Current(adjust.try_into().unwrap()))?;
        }
        Ok(())
    }

    fn parse_token(&mut self) -> Result<Token, ParseError> {
        self.align()?;
        let token = self.reader.read_u32::<BigEndian>()?;

        let parsed = match token {
            0x1 => {
                let mut buf: Vec<u8> = vec![];
                self.reader.read_until(0, &mut buf)?;
                if *buf.last().unwrap_or(&1) != 0 {
                    // TODO(simonshields): make a better error here.
                    return Err(ParseError::StringNotTerminated(0xffff));
                }

                let c_string = CStr::from_bytes_with_nul(&buf)?;
                Token::BeginNode(c_string.to_str()?.to_owned())
            }
            0x2 => Token::EndNode,
            0x3 => {
                let len = self.reader.read_u32::<BigEndian>()?.try_into().unwrap();
                let nameoff = self.reader.read_u32::<BigEndian>()?;
                let key = self.string_at(nameoff.try_into().unwrap())?;
                let mut value = vec![0; len];

                self.reader.read_exact(value.as_mut_slice())?;

                Token::Prop(Property { key, value })
            }
            0x4 => Token::Nop,
            0x9 => Token::End,
            t => {
                return Err(ParseError::InvalidToken(
                    self.offset()? - std::mem::size_of::<u32>() as u64,
                    t,
                ))
            }
        };

        Ok(parsed)
    }

    fn parse_node(&mut self, name: String) -> Result<Rc<Node>, ParseError> {
        let mut can_end = false;
        let mut children = vec![];
        let mut properties = vec![];
        loop {
            match self.parse_token()? {
                Token::BeginNode(name) => {
                    children.push(self.parse_node(name)?);
                    can_end = true;
                }
                Token::EndNode => break,
                Token::End => {
                    if can_end {
                        self.reader.seek(SeekFrom::Current(-4))?;
                        break;
                    } else {
                        return Err(ParseError::UnexpectedToken(Token::End));
                    }
                }
                Token::Prop(prop) => {
                    properties.push(prop);
                    can_end = false;
                }
                Token::Nop => {}
            }
        }

        Ok(Rc::new(Node {
            name,
            children,
            properties,
        }))
    }

    pub fn parse(&mut self) -> Result<Rc<Node>, ParseError> {
        loop {
            match self.parse_token()? {
                Token::BeginNode(name) => {
                    return self.parse_node(name);
                }
                Token::Nop => {}
                t => return Err(ParseError::UnexpectedToken(t)),
            }
        }
    }
}

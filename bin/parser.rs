#![allow(unused_imports)]

use std::collections::HashMap;
use std::fs::File;
use std::io;
use std::io::prelude::*;
use std::iter::Peekable;
use std::path::{Path, PathBuf};
use std::str;
use std::str::CharIndices;

use regalloc::{Reg, RegClass};

use crate::test_framework::{
  i_add, i_addm, i_cmp_gt, i_cmp_le, i_cmp_lt, i_copy, i_finish, i_goto,
  i_goto_ctf, i_imm, i_load, i_print_i, i_print_s, i_store, i_sub, i_subm,
  s_add, s_addm, s_and, s_cmp_eq, s_cmp_ge, s_cmp_gt, s_cmp_le, s_cmp_lt,
  s_copy, s_fadd, s_fdiv, s_fmul, s_fsub, s_if_then, s_if_then_else, s_imm,
  s_immf, s_load, s_loadf, s_mod, s_mul, s_print_f, s_print_i, s_print_s,
  s_repeat_until, s_shr, s_store, s_storef, s_sub, s_while_do, Blockifier,
  Func, Inst, AM, AM_R, AM_RI, AM_RR, RI, RI_I, RI_R,
};

#[derive(Debug)]
pub enum ParseError {
  IoError(io::Error),
  Parse(String),
}

impl From<io::Error> for ParseError {
  fn from(err: io::Error) -> ParseError {
    ParseError::IoError(err)
  }
}

pub type ParseResult<T> = Result<T, ParseError>;

pub fn parse_file(path: PathBuf) -> ParseResult<Func> {
  let basename = path.file_stem().unwrap().to_str().unwrap().to_string();
  let mut file = File::open(path)?;
  let mut content = String::new();
  file.read_to_string(&mut content)?;
  parse_content(&basename, &content)
}

struct Parser<'f, 'str> {
  func: &'f mut Func,
  vars: HashMap<String, Reg>,

  source: &'str str,
  iter: Peekable<CharIndices<'str>>,
  line: usize,
  start: usize,
  current: usize,
}

impl<'f, 'str> Parser<'f, 'str> {
  fn new(func: &'f mut Func, source: &'str str) -> Self {
    let iter = source.char_indices().peekable();
    Self {
      func,
      start: 0,
      current: 0,
      line: 1,
      vars: HashMap::new(),
      iter,
      source,
    }
  }

  // Environment.
  fn define_virtual_var(
    &mut self, name: &str, reg_class: RegClass,
  ) -> ParseResult<()> {
    if let Some(_) =
      self.vars.insert(name.into(), self.func.new_virtual_reg(reg_class))
    {
      self.error("duplicate variable declaration")
    } else {
      Ok(())
    }
  }

  fn define_real_var(
    &mut self, name: &str, reg_class: RegClass, index: u8,
  ) -> ParseResult<()> {
    if let Some(_) =
      self.vars.insert(name.into(), Reg::new_real(reg_class, 0x0, index))
    {
      self.error("duplicate variable declaration")
    } else {
      Ok(())
    }
  }

  fn var(&mut self, name: &str) -> Option<Reg> {
    self.vars.get(name).cloned()
  }

  fn define_block(&mut self, block_name: String, insts: Vec<Inst>) {
    self.func.block(&block_name, insts);
  }

  // Parsing.
  fn peek(&mut self) -> Option<char> {
    if let Some((_, c)) = self.iter.peek() {
      Some(*c)
    } else {
      None
    }
  }

  fn peek_next(&self) -> Option<char> {
    let mut clone = self.iter.clone();
    if let Some(_) = clone.next() {
      if let Some((_, c)) = clone.peek() {
        return Some(*c);
      }
    }
    None
  }

  fn advance(&mut self) -> Option<char> {
    if let Some((i, ch)) = self.iter.next() {
      self.current = i;
      Some(ch)
    } else {
      None
    }
  }

  // Higher level parsing.
  fn skip_whitespace_and_comments(&mut self) {
    while let Some(c) = self.peek() {
      if c == ' ' || c == '\t' || c == '\r' || c == '\n' {
        // Skip whitespace.
        self.advance().unwrap();
        if c == '\n' {
          self.line += 1;
        }
      } else if c == ';' {
        // It's a comment! skip until the end of line.
        self.advance().unwrap();
        while let Some(c) = self.advance() {
          if c == '\n' {
            self.line += 1;
            break;
          }
        }
      } else {
        break;
      }
    }
  }

  fn read_char(&mut self) -> ParseResult<char> {
    self.skip_whitespace_and_comments();
    if let Some(c) = self.advance() {
      Ok(c)
    } else {
      self.error("expected char")
    }
  }

  fn try_read_ident(&mut self) -> Option<String> {
    self.skip_whitespace_and_comments();

    if let Some(c) = self.peek() {
      if !is_alpha(c) {
        return None;
      }
      self.advance().unwrap();
    } else {
      return None;
    }

    self.start = self.current;
    while let Some(c) = self.peek() {
      if !is_alpha_numeric(c) {
        break;
      }
      self.advance().unwrap();
    }

    let substr =
      str::from_utf8(&self.source.as_bytes()[self.start..self.current + 1])
        .unwrap();
    Some(substr.to_string())
  }

  fn read_ident(&mut self) -> ParseResult<String> {
    if let Some(string) = self.try_read_ident() {
      Ok(string)
    } else {
      self.error("expected identifier or keyword")
    }
  }

  fn read_string(&mut self) -> ParseResult<&str> {
    self.skip_whitespace_and_comments();
    if let Some('"') = self.advance() {
      // All good!
    } else {
      return self.error("expected opening \"");
    }
    self.start = self.current;
    while let Some(c) = self.peek() {
      if c == '\n' {
        self.line += 1;
      }
      self.advance().unwrap();
      if c == '"' {
        let substr =
          str::from_utf8(&self.source.as_bytes()[self.start + 1..self.current])
            .unwrap();
        return Ok(substr);
      }
    }
    self.error("unterminated string")
  }

  fn try_read_number(&mut self) -> ParseResult<Option<f64>> {
    self.skip_whitespace_and_comments();
    let first_digit = if let Some(c) = self.peek() {
      if is_digit(c) {
        self.advance().unwrap();
        c
      } else {
        return Ok(None);
      }
    } else {
      return Ok(None);
    };
    let mut number = first_digit.to_digit(10).unwrap() as f64;
    let mut fractional_power_of_ten: Option<f64> = None;
    while let Some(c) = self.peek() {
      if is_digit(c) {
        self.advance().unwrap();
        let c_num = c.to_digit(10).unwrap() as f64;
        if let Some(decimal) = fractional_power_of_ten.as_mut() {
          number += c_num * *decimal;
          *decimal /= 10.0;
        } else {
          number *= 10.0;
          number += c_num;
        }
      } else if c == '.' {
        if let Some(d) = self.peek_next() {
          if is_digit(d) {
            self.advance().unwrap();
            if fractional_power_of_ten.is_some() {
              return self.error("unexpected dot in number")?;
            }
            fractional_power_of_ten = Some(0.1);
            continue;
          }
        }
        break;
      } else {
        break;
      }
    }
    Ok(Some(number))
  }
  fn _read_number(&mut self) -> ParseResult<f64> {
    Ok(self.try_read_number()?.expect("expected number"))
  }

  fn try_read_int(&mut self) -> ParseResult<Option<u32>> {
    if let Some(value) = self.try_read_number()? {
      let as_int = value as u32;
      let as_float_back = as_int as f64;
      if as_float_back != value {
        self.error("expected an integer, got a double")
      } else {
        Ok(Some(as_int))
      }
    } else {
      Ok(None)
    }
  }
  fn read_int(&mut self) -> ParseResult<u32> {
    Ok(self.try_read_int()?.expect("expected integer"))
  }

  fn try_read_var(&mut self) -> ParseResult<Option<Reg>> {
    if let Some(ident) = self.try_read_ident() {
      if let Some(reg) = self.vars.get(&ident) {
        Ok(Some(*reg))
      } else {
        self.error("expected variable")
      }
    } else {
      Ok(None)
    }
  }
  fn read_var(&mut self) -> ParseResult<Reg> {
    Ok(self.try_read_var()?.expect("expected var"))
  }

  fn to_reg_class(&self, ident: &str) -> ParseResult<RegClass> {
    if ident == "i32" {
      Ok(RegClass::I32)
    } else if ident == "f32" {
      Ok(RegClass::F32)
    } else {
      self.error("unknown register class")
    }
  }

  fn read_ri(&mut self) -> ParseResult<RI> {
    if let Some(reg) = self.try_read_var()? {
      Ok(RI_R(reg))
    } else {
      let int = self.read_int()?;
      Ok(RI_I(int))
    }
  }

  fn read_am(&mut self) -> ParseResult<AM> {
    // Either RR or RI. As a shortcut, allow R, meaning RI with 0 offset.
    self.expect_char('[')?;
    let base = self.read_var()?;
    let am = if let Some(disp) = self.try_read_var()? {
      AM_RR(base, disp)
    } else if let Some(offset) = self.try_read_int()? {
      AM_RI(base, offset)
    } else {
      AM_R(base)
    };
    self.expect_char(']')?;
    Ok(am)
  }

  fn expect_char(&mut self, expected: char) -> ParseResult<()> {
    let c = self.read_char()?;
    if c != expected {
      self.error(&format!("expected char '{}'", expected))
    } else {
      Ok(())
    }
  }

  fn is_done(&mut self) -> bool {
    self.skip_whitespace_and_comments();
    self.peek().is_none()
  }

  fn error<T>(&self, msg: &str) -> ParseResult<T> {
    Err(ParseError::Parse(format!("error at line {}: {}", self.line, msg)))
  }
}

fn parse_content(func_name: &str, content: &str) -> ParseResult<Func> {
  let mut func = Func::new(func_name);

  let mut parser = Parser::new(&mut func, content);

  // Look for variable declarations.
  let mut name;
  loop {
    name = parser.read_ident()?;
    let c = parser.read_char()?;
    if c == '=' {
      // variable declaration.
      let real_or_class = parser.read_ident()?;
      if real_or_class == "real" {
        let class = parser.read_ident()?;
        let reg_class = parser.to_reg_class(&class)?;
        let index = parser.read_int()?;
        if index > 255 {
          return parser.error("expected u8");
        }
        parser.define_real_var(&name, reg_class, index as u8)?;
      } else {
        let reg_class = parser.to_reg_class(&real_or_class)?;
        parser.define_virtual_var(&name, reg_class)?;
      }
    } else if c == ':' {
      // first block declaration!
      break;
    } else {
      return parser.error("expected = or :");
    }
  }

  parser.func.set_entry(&name);
  let mut next_block_name = Some(name);

  // Look for blocks (name already contains the name of the first block).
  loop {
    let mut insts = Vec::new();
    let block_name = next_block_name;

    // Look for instructions.
    loop {
      // Either:
      // - nothing (empty block, no more blocks thereafter).
      // - instruction (name, maybe operands).
      // - next block.
      if parser.is_done() {
        next_block_name = None;
        break;
      }

      let inst_or_block_name = parser.read_ident()?;
      match inst_or_block_name.as_str() {
        "add" => {
          let dst = parser.read_var()?;
          parser.expect_char(',')?;
          let src = parser.read_var()?;
          parser.expect_char(',')?;
          let op = parser.read_ri()?;
          insts.push(i_add(dst, src, op));
        }

        "cmp_lt" => {
          let dst = parser.read_var()?;
          parser.expect_char(',')?;
          let src = parser.read_var()?;
          parser.expect_char(',')?;
          let ri = parser.read_ri()?;
          insts.push(i_cmp_lt(dst, src, ri));
        }

        "finish" => {
          let return_val =
            parser.try_read_ident().and_then(|var_name| parser.var(&var_name));
          insts.push(i_finish(return_val));
        }

        "goto_ctf" => {
          let test_var = parser.read_var()?;
          parser.expect_char(',')?;
          let then_block = parser.read_ident()?;
          parser.expect_char(',')?;
          let else_block = parser.read_ident()?;
          insts.push(i_goto_ctf(test_var, &then_block, &else_block));
        }

        "goto" => {
          let target = parser.read_ident()?;
          insts.push(i_goto(&target));
        }

        "imm" => {
          let dst = parser.read_var()?;
          parser.expect_char(',')?;
          let value = parser.read_int()?;
          insts.push(i_imm(dst, value));
        }

        "load" => {
          let dst = parser.read_var()?;
          parser.expect_char(',')?;
          let addr = parser.read_am()?;
          insts.push(i_load(dst, addr));
        }

        "store" => {
          let addr = parser.read_am()?;
          parser.expect_char(',')?;
          let src = parser.read_var()?;
          insts.push(i_store(addr, src));
        }

        "printi" => {
          let reg = parser.read_var()?;
          insts.push(i_print_i(reg));
        }

        "prints" => {
          let string = parser.read_string()?;
          insts.push(i_print_s(string));
        }

        "println" => {
          let string = parser.read_string()?;
          insts.push(i_print_s(&format!("{}\n", string)));
        }

        _ => {
          next_block_name = Some(inst_or_block_name);
          break;
        }
      }
    }

    if let Some(block_name) = block_name {
      parser.define_block(block_name, insts);
    }

    if parser.is_done() {
      break;
    }

    if parser.read_char()? != ':' {
      return parser.error(
        "expected : after possible block name, or unexpected instruction name",
      );
    }
  }

  func.finish();
  Ok(func)
}

fn is_digit(c: char) -> bool {
  c >= '0' && c <= '9'
}

fn is_alpha(c: char) -> bool {
  c >= 'a' && c <= 'z' || c >= 'A' && c <= 'Z' || c == '_' || c == '-'
}

fn is_alpha_numeric(c: char) -> bool {
  is_digit(c) || is_alpha(c)
}

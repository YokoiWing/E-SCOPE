use egg::{Id, Language, Symbol, define_language};
use std::fmt::Debug;
use std::hash::Hash;

pub trait LanguageType: Debug + Clone + PartialEq + Eq + Hash {
    type Lang: Language;

    fn from_lang(lang: Self::Lang) -> Self;

    fn from_op(op: &str) -> Self;

    fn to_lang_input(self: &Self) -> Self::Lang;

    fn to_lang_output(self: &Self, input: Id) -> Self::Lang;

    fn to_lang_gate(self: &Self, inputs: Vec<Id>) -> Self::Lang;

    /// Constants have no fanin even when they are not primary inputs.
    fn is_constant(&self) -> bool;
}

define_language! {
    pub enum AigLanguage {
        Bool(bool),
        "and" = And([Id; 2]),
        "not" = Not(Id),
        Output(Symbol, Id),
        Input(Symbol),
        Symbol(Symbol),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AigType {
    Bool(bool),
    And,
    Not,
    Symbol(Symbol),
}

impl LanguageType for AigType {
    type Lang = AigLanguage;

    fn from_lang(lang: Self::Lang) -> Self {
        match lang {
            AigLanguage::Bool(b) => AigType::Bool(b),
            AigLanguage::And(_) => AigType::And,
            AigLanguage::Not(_) => AigType::Not,
            AigLanguage::Output(s, _) => AigType::Symbol(s),
            AigLanguage::Input(s) => AigType::Symbol(s),
            AigLanguage::Symbol(s) => AigType::Symbol(s),
        }
    }

    fn from_op(op: &str) -> Self {
        match op {
            "and" => AigType::And,
            "not" => AigType::Not,
            "true" => AigType::Bool(true),
            "false" => AigType::Bool(false),
            s => AigType::Symbol(s.to_owned().into()),
        }
    }

    fn to_lang_input(self: &Self) -> Self::Lang {
        match *self {
            AigType::Bool(b) => AigLanguage::Bool(b),
            AigType::Symbol(s) => AigLanguage::Input(s),
            _ => panic!("For input, op type (AigType) should be Bool or Symbol"),
        }
    }

    fn to_lang_output(self: &Self, input: Id) -> Self::Lang {
        match *self {
            AigType::Symbol(s) => AigLanguage::Output(s, input),
            _ => panic!("For output, op type (AigType) should be Symbol"),
        }
    }

    fn to_lang_gate(self: &Self, inputs: Vec<Id>) -> Self::Lang {
        match *self {
            AigType::And => AigLanguage::And(inputs.try_into().unwrap()),
            AigType::Not => AigLanguage::Not(inputs[0]),
            _ => panic!("For gate, op type (AigType) should be And or Not"),
        }
    }

    fn is_constant(&self) -> bool {
        matches!(self, AigType::Bool(_))
    }
}

impl From<AigLanguage> for AigType {
    fn from(aig: AigLanguage) -> Self {
        AigType::from_lang(aig)
    }
}

define_language! {
    pub enum StdCellLanguage {
        Bool(bool),
        Gate(Symbol, Box<[Id]>),
        Output(Symbol, Id),
        Input(Symbol),
        Symbol(Symbol),
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum StdCellType {
    Bool(bool),
    Symbol(Symbol),
}

impl LanguageType for StdCellType {
    type Lang = StdCellLanguage;

    fn from_lang(lang: StdCellLanguage) -> Self {
        match lang {
            StdCellLanguage::Bool(b) => StdCellType::Bool(b),
            StdCellLanguage::Gate(s, _) => StdCellType::Symbol(s),
            StdCellLanguage::Output(s, _) => StdCellType::Symbol(s),
            StdCellLanguage::Input(s) => StdCellType::Symbol(s),
            StdCellLanguage::Symbol(s) => StdCellType::Symbol(s),
        }
    }

    fn from_op(op: &str) -> Self {
        match op {
            "true" => StdCellType::Bool(true),
            "false" => StdCellType::Bool(false),
            s => StdCellType::Symbol(s.to_owned().into()),
        }
    }

    fn to_lang_input(self: &Self) -> Self::Lang {
        match *self {
            StdCellType::Bool(b) => StdCellLanguage::Bool(b),
            StdCellType::Symbol(s) => StdCellLanguage::Input(s),
        }
    }

    fn to_lang_output(self: &Self, input: Id) -> Self::Lang {
        match *self {
            StdCellType::Symbol(s) => StdCellLanguage::Output(s, input),
            _ => panic!("For output, op type (StdCellLanguage) should be Symbol"),
        }
    }

    fn to_lang_gate(self: &Self, inputs: Vec<Id>) -> Self::Lang {
        match *self {
            StdCellType::Symbol(s) => StdCellLanguage::Gate(s, inputs.into()),
            _ => panic!("For gate, op type (StdCellLanguage) should be Symbol"),
        }
    }

    fn is_constant(&self) -> bool {
        matches!(self, StdCellType::Bool(_))
    }
}

impl StdCellType {
    pub fn to_string(self: &Self) -> String {
        match self {
            Self::Bool(b) => if *b { "1" } else { "0" }.to_string(),
            Self::Symbol(s) => s.to_string(),
        }
    }

    pub fn to_string_as_io(self: &Self) -> Option<String> {
        match self {
            Self::Bool(_b) => None,
            Self::Symbol(s) => Some(s.to_string()),
        }
    }
}

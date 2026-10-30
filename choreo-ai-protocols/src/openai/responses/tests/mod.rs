//! The Responses adapter's test suite, one module per subject.
//!
//! Split by what each case is *about* rather than by file size, so a change to
//! one concern fails in one place: the wire shapes ([`wire`]), the history →
//! `input` lowering ([`input`]), the surrounding request body and its two
//! dialects ([`body`]), the response → turn conversion and reasoning artifact
//! ([`turn`]), and the streaming tool-call accumulator ([`stream`]).
//!
//! Each subject module imports the items it names directly from the sibling
//! `responses` module that defines them (or from the `openai` parent), so no
//! shared prelude has to be kept in sync here.

mod body;
mod input;
mod stream;
mod turn;
mod wire;

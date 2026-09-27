// Encoding must be its own bundle, loaded first. Bundlers run every module
// initializer before the entry body, and other platform packages construct a
// TextEncoder at module scope — assigning it at the end is already too late.
//
// This is deliberately the *browser* build. The package's Node build encodes
// through Buffer, which does not exist here in any useful form, and fails at
// encode() time with "not a constructor" rather than at load.
import 'fastestsmallesttextencoderdecoder/EncoderDecoderTogether.min.js';

// `frances:v1/sections` — transcript proxy + frame constructors.
//
// Every frame is one-shot: construct it, `transcript.push(it)`, done.
// Streaming lives in entities (`frances:v1/entities`), referenced from
// the transcript by an `EntityRefSection`.

export const {
  transcript,
  ErrorSection,
  JsonSection,
  DiffSection,
  EntityRefSection,
} = globalThis.__frances_v1_stash__;

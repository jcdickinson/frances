// `frances:v1/storage` — per-workflow SQL surface.
//
// Wraps the Rust-side `Db` singleton + `Tx` class with the user-facing
// shape:
//
//   import { db } from "frances:v1/storage";
//   await db.exec(sql, params);
//   const rows = await db.query(sql, params, { signal });
//   for await (const row of db.queryStream(sql, params, { signal })) { ... }
//   await db.transaction(async tx => { ... });
//
// - `exec(sql, params)` → Promise<{ rowsAffected, lastInsertRowid }>
// - `query(sql, params, opts?)` → Promise<Row[]>
// - `queryStream(sql, params, opts?)` → ReadableStream<Row>
//     `opts.signal` (an AbortSignal) is checked before each pull; an
//     aborted signal errors the stream with the signal's reason. The
//     in-flight Rust-side `_innerQueryStream` call (and the first
//     `next()`) are NOT interruptible — turso doesn't expose a cancel
//     primitive — so abort takes effect between rows.
// - `transaction(cb)` runs `cb(tx)` inside `BEGIN/COMMIT`. The tx
//     mirrors `db`'s reading methods, plus `tx.commit()` / `tx.rollback()`
//     escape hatches that decouple settlement from control flow. If the
//     callback throws, the tx rolls back (unless already settled) and
//     the throw propagates; if it resolves, the tx commits (unless
//     already settled) and the resolved value bubbles up. Settling
//     explicitly first means the host doesn't try to settle again —
//     `commit()` followed by `throw` still throws, `rollback()`
//     followed by `return v` still returns `v`.

import { ReadableStream } from "whatwg:web-streams";

const { db: _rawDb } = globalThis.__frances_v1_stash__;

const db = wrapDb(_rawDb);

function wrapDb(raw) {
  return {
    exec(sql, params) {
      return raw._exec(sql, normalizeParams(params));
    },
    query(sql, params, _opts) {
      // `_opts.signal` is accepted for API symmetry but query is a
      // one-shot — the underlying turso call has no interrupt, so the
      // signal can't shorten it. queryStream is the cancellable path.
      return raw._query(sql, normalizeParams(params));
    },
    queryStream(sql, params, opts) {
      return makeRowStream(raw, sql, normalizeParams(params), opts);
    },
    async transaction(cb) {
      const rawTx = await raw._beginTransaction();
      const tx = wrapTx(rawTx);
      let result;
      let threw;
      let didThrow = false;
      try {
        result = await cb(tx);
      } catch (e) {
        threw = e;
        didThrow = true;
      }
      if (didThrow) {
        try {
          await rawTx._settleDefault(false);
        } catch (_) {
          // Already-settled or rollback failure — surface the user's
          // original throw, not the settle error.
        }
        throw threw;
      }
      await rawTx._settleDefault(true);
      return result;
    },
  };
}

function wrapTx(raw) {
  return {
    exec(sql, params) {
      return raw._exec(sql, normalizeParams(params));
    },
    query(sql, params, _opts) {
      return raw._query(sql, normalizeParams(params));
    },
    queryStream(sql, params, opts) {
      return makeRowStream(raw, sql, normalizeParams(params), opts);
    },
    commit() {
      return raw._commit();
    },
    rollback() {
      return raw._rollback();
    },
  };
}

function normalizeParams(params) {
  if (params === undefined || params === null) return [];
  return params;
}

function makeRowStream(raw, sql, params, opts) {
  const signal = opts && opts.signal;
  let inner;
  let started = false;
  let streamController;

  const stream = new ReadableStream({
    start(c) {
      streamController = c;
    },
    async pull(controller) {
      if (signal && signal.aborted) {
        controller.error(signal.reason);
        return;
      }
      if (!started) {
        try {
          inner = await raw._innerQueryStream(sql, params);
          started = true;
        } catch (e) {
          controller.error(e);
          return;
        }
      }
      try {
        const { done, value } = await inner.next();
        if (signal && signal.aborted) {
          controller.error(signal.reason);
          return;
        }
        if (done) controller.close();
        else controller.enqueue(value);
      } catch (e) {
        controller.error(e);
      }
    },
  });

  if (signal) {
    const onAbort = () => {
      try {
        streamController.error(signal.reason);
      } catch (_) {
        // Stream already closed or errored — nothing to do.
      }
    };
    if (signal.aborted) onAbort();
    else signal.addEventListener("abort", onAbort);
  }

  return stream;
}

export { db };

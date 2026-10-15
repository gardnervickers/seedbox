'use strict';
// V8 probes: WebAssembly out-of-bounds traps (the trap handler turns a guard-region SIGSEGV
// into a RuntimeError) from Liftoff and TurboFan code, and a JIT-heavy JavaScript workload
// (inline caches, deoptimization, on-stack replacement, regexp tier-up, code churn); worker
// threads passing a token through shared memory (workersToken).
//
// Usage: jsfz wasm|jit|workersToken SEED SCALE

let digest = 0x811c9dc5;
function emit(key, value) {
  const line = `${key} ${value}`;
  console.log(line);
  for (let i = 0; i < line.length; i++) digest = Math.imul(digest ^ line.charCodeAt(i), 16777619);
  digest = Math.imul(digest ^ 10, 16777619);
}
function fail(msg) {
  console.log(`FAIL ${msg}`);
  process.exit(1);
}
const mix = (h, x) => Math.imul(h ^ (x | 0), 16777619) ^ (h >>> 13);
const hex = (h) => (h >>> 0).toString(16).padStart(8, '0');

let rs = 1;
function rnd() { // xorshift32
  rs ^= rs << 13; rs >>>= 0;
  rs ^= rs >>> 17;
  rs ^= rs << 5; rs >>>= 0;
  return rs;
}

// ---------------------------------------------------------------------------------------------
// wasm: a hand-assembled module.

function leb(n) {
  const b = [];
  do {
    let byte = n & 0x7f;
    n = Math.floor(n / 128);
    if (n) byte |= 0x80;
    b.push(byte);
  } while (n);
  return b;
}
const vec = (items) => [...leb(items.length), ...items.flat()];
const name = (s) => [...leb(s.length), ...Buffer.from(s)];
const section = (id, bytes) => [id, ...leb(bytes.length), ...bytes];
const I32 = 0x7f, I64 = 0x7e, V128 = 0x7b;
const get = (i) => [0x20, i], set = (i) => [0x21, i], i32c = (v) => [0x41, ...sleb(v)];
function sleb(n) {
  const b = [];
  for (;;) {
    const byte = n & 0x7f;
    n >>= 7;
    if ((n === 0 && !(byte & 0x40)) || (n === -1 && (byte & 0x40))) { b.push(byte); return b; }
    b.push(byte | 0x80);
  }
}

// [export name, params, results, locals, body]
const FUNCS = [
  ['load', [I32], [I32], [], [...get(0), 0x28, 2, 0]],
  ['store', [I32, I32], [], [], [...get(0), ...get(1), 0x36, 2, 0]],
  ['load64', [I32], [I64], [], [...get(0), 0x29, 3, 0]],
  ['load8far', [I32], [I32], [], [...get(0), 0x2d, 0, ...leb(1000)]],
  // sum(a, n): n consecutive i32 loads from a, so a range crossing the end traps mid-loop.
  ['sum', [I32, I32], [I32], [[2, I32]], [
    0x02, 0x40, 0x03, 0x40,
    ...get(2), ...get(1), 0x4f, 0x0d, 1,
    ...get(3), ...get(0), ...get(2), ...i32c(2), 0x74, 0x6a, 0x28, 2, 0, 0x6a, ...set(3),
    ...get(2), ...i32c(1), 0x6a, ...set(2),
    0x0c, 0, 0x0b, 0x0b, ...get(3)]],
  ['div', [I32, I32], [I32], [], [...get(0), ...get(1), 0x6d]],
  ['trap', [], [], [], [0x00]],
  ['grow', [I32], [I32], [], [...get(0), 0x40, 0]],
  ['size', [], [I32], [], [0x3f, 0]],
  ['callind', [I32, I32], [I32], [], [...get(1), ...get(0), 0x11, 10, 0]],
  ['inc', [I32], [I32], [], [...get(0), ...i32c(1), 0x6a]],
  ['dbl', [I32], [I32], [], [...get(0), ...i32c(2), 0x6c]],
  ['rec', [I32], [I32], [], [...get(0), ...i32c(1), 0x6a, 0x10, 12]],
  ['simd', [I32], [I32], [[1, V128]], [
    ...get(0), 0xfd, 0x00, 4, 0, ...set(1),
    ...get(1), ...get(1), 0xfd, 0xae, 0x01, ...set(1),
    ...get(1), 0xfd, 0x1b, 0, ...get(1), 0xfd, 0x1b, 3, 0x73]],
  ['fill', [I32, I32, I32], [], [], [...get(0), ...get(1), ...get(2), 0xfc, 0x0b, 0]],
  ['store8huge', [I32, I32], [], [], [...get(0), ...get(1), 0x3a, 0, ...leb(0xfffffff0)]],
];

function moduleBytes() {
  const types = FUNCS.map(([, p, r]) => [0x60, ...vec(p), ...vec(r)]);
  const bodies = FUNCS.map(([, , , locals, code]) => {
    const body = [...vec(locals.map(([n, t]) => [n, t])), ...code, 0x0b];
    return [...leb(body.length), ...body];
  });
  const exports = FUNCS.map(([n], i) => [...name(n), 0x00, i]);
  exports.push([...name('mem'), 0x02, 0]);
  const idx = (n) => FUNCS.findIndex((f) => f[0] === n);
  return Uint8Array.from([
    0x00, 0x61, 0x73, 0x6d, 0x01, 0x00, 0x00, 0x00,
    ...section(1, vec(types)),
    ...section(3, vec(FUNCS.map((_, i) => leb(i)))),
    ...section(4, vec([[0x70, 0x00, 8]])),
    ...section(5, vec([[0x01, 1, 16]])),
    ...section(7, vec(exports)),
    // Table entries 0-3: inc, dbl, load and a function of the wrong type; 4-7 are null.
    ...section(9, vec([[0x00, 0x41, 0, 0x0b, ...vec([idx('inc'), idx('dbl'), idx('load'), idx('div')].map(leb))]])),
    ...section(10, vec(bodies)),
  ]);
}

function wasm(seed, scale) {
  const e = new WebAssembly.Instance(new WebAssembly.Module(moduleBytes())).exports;
  const counts = new Map();
  const trap = (fn) => {
    try {
      return fn();
    } catch (err) {
      const key = `${err.constructor.name}:${err.message}`.replace(/ /g, '_');
      counts.set(key, (counts.get(key) || 0) + 1);
      return 'trap';
    }
  };
  let h = 0x811c9dc5;
  const note = (v) => { h = mix(h, typeof v === 'bigint' ? Number(v & 0xffffffffn) : v === 'trap' ? 7777 : v === undefined ? 1 : v); };
  for (let i = 0; i < 100000 * scale; i++) {
    const r = rnd();
    const size = e.size() * 65536;
    const near = size - 8 + (r >>> 20) % 16; // around the end of memory
    const far = (r >>> 4) % 4 === 0 ? 0xffffffff - (r >>> 24) : size + ((r >>> 4) % 0x100000);
    switch (r % 12) {
      case 0: { // aligned in-bounds store, then load back
        const a = (rnd() % (size / 4)) * 4, v = rnd() | 0;
        e.store(a, v);
        if (e.load(a) !== v) fail(`wasm load ${a} after store`);
        note(v);
        break;
      }
      case 1: note(trap(() => e.load(near))); break;
      case 2: note(trap(() => e.store(far, r))); break;
      case 3: note(trap(() => e.load8far(size - 1004 + (r >>> 24) % 8))); break;
      case 4: note(trap(() => e.sum(size - 4 * (1 + (r >>> 8) % 300), 1 + (r >>> 16) % 300))); break;
      case 5: note(trap(() => e.div(r | 0, [0, -1, 3, 0][(r >>> 8) % 4]))); note(trap(() => e.div(-0x80000000, -1))); break;
      case 6: note(trap(() => e.trap())); break;
      case 7: note(trap(() => e.callind((r >>> 8) % 10, r & 0xffff))); break;
      case 8: note(trap(() => e.simd(size - 32 + (r >>> 8) % 24))); break;
      case 9: note(trap(() => e.fill(size - 64 + (r >>> 8) % 128, r & 0xff, 32))); break;
      case 10: note(trap(() => e.store8huge(r >>> 8, r))); break;
      case 11: note(trap(() => e.load64(near))); break;
    }
    if (i % 10000 === 9999 && e.size() < 16) note(e.grow(1));
  }
  for (let k = 0; k < 5; k++) note(trap(() => e.rec(0)));
  // Final memory contents, read back through a JavaScript view.
  const words = new Int32Array(e.mem.buffer);
  for (let i = 0; i < words.length; i += 61) h = mix(h, words[i]);
  emit('pages', e.size());
  for (const k of [...counts.keys()].sort()) emit(`trap.${k}`, counts.get(k));
  emit('wasm', hex(h));
}

// ---------------------------------------------------------------------------------------------
// jit

class Shape { constructor(x) { this.x = x; } area() { return this.x; } }
class Sq extends Shape { area() { return this.x * this.x; } }
class Rect extends Shape { constructor(x, y) { super(x); this.y = y; } area() { return this.x * this.y; } }
class Tri extends Rect { area() { return (this.x * this.y) >> 1; } }
class Circ extends Shape { area() { return Math.floor(this.x * this.x * 3.14159); } }
class Ring extends Circ { constructor(x, w) { super(x); this.w = w; } area() { return super.area() - this.w; } }

function shapes(n) {
  const all = [];
  for (let i = 0; i < 64; i++) {
    const r = rnd(), x = r % 100, y = (r >>> 8) % 100;
    // Object literals with different property orders give different maps at one site.
    all.push([new Shape(x), new Sq(x), new Rect(x, y), new Tri(x, y), new Circ(x), new Ring(x, y),
      { x, area() { return x + 1; } }, { y, x, area() { return y - x; } }][r % 8]);
  }
  let h = 0;
  for (let i = 0; i < n; i++) h = mix(h, all[i & 63].area());
  return h;
}

// add sees integers, then doubles, then strings, then BigInts: each change deoptimizes it.
function add(a, b) { return a + b; }
function deopts(n) {
  let h = 0, s = '', big = 0n;
  for (let i = 0; i < n; i++) h = mix(h, add(i, i >> 1));
  let f = 0;
  for (let i = 0; i < n; i++) f = add(f, i * 0.5);
  for (let i = 0; i < n / 50; i++) s = add(s, String.fromCharCode(97 + i % 26)).slice(-64);
  for (let i = 0; i < n / 50; i++) big = add(big, BigInt(i) * 12345678901n) % 0xffffffffffffffffn;
  h = mix(h, f | 0);
  for (let i = 0; i < s.length; i++) h = mix(h, s.charCodeAt(i));
  return mix(h, Number(big % 1000000007n));
}

// One long-running call: only on-stack replacement can optimize it.
function osr(n) {
  const a = new Float64Array(1024), b = new Int32Array(1024);
  let acc = 0;
  for (let i = 0; i < n; i++) {
    const j = i & 1023;
    a[j] = a[j] * 0.5 + Math.sqrt(j + 1) + (i % 7);
    b[j] = (b[j] ^ (i * 2654435761)) + (a[j] | 0);
    acc = (acc + b[j]) | 0;
  }
  return acc;
}

function strings(n) {
  let text = '';
  for (let i = 0; i < n; i++) {
    const r = rnd();
    text += `${r % 1000}-${(r >>> 10).toString(36)} `;
  }
  // The same regexp runs often enough to tier up from bytecode to native code.
  const re = /(\d+)-([a-z]+)(\d*)/g;
  let h = 0, m;
  for (let k = 0; k < 4; k++) {
    re.lastIndex = 0;
    while ((m = re.exec(text)) !== null) h = mix(h, (m[1].length << 8) + m[2].length + (m[3] ? +m[3][0] : 0));
  }
  const words = text.split(' ').filter(Boolean).sort((p, q) => (p.length - q.length) || (p < q ? -1 : p > q ? 1 : 0));
  for (const w of words.slice(0, 200)) h = mix(h, w.charCodeAt(w.length - 1));
  const json = JSON.parse(JSON.stringify({ words: words.slice(0, 500), n }));
  return mix(h, json.words.join('').length);
}

function* gen(n) { for (let i = 0; i < n; i++) yield i * i; }
async function chain(n) {
  let s = 0;
  for (let i = 0; i < n; i++) s = (s + await Promise.resolve(i)) | 0;
  return s;
}

function exceptions(n) {
  let h = 0;
  const thrower = (i) => { if (i % 3 === 0) throw new RangeError(`r${i % 10}`); return i; };
  for (let i = 0; i < n; i++) {
    try { h = mix(h, thrower(i)); } catch (e) { h = mix(h, e.message.charCodeAt(1)); }
  }
  try { (function down(d) { return down(d + 1) + 1; })(0); } catch (e) { h = mix(h, e instanceof RangeError ? 1 : 2); }
  return h;
}

// Thousands of distinct functions, each run hot enough to be compiled and then dropped.
function churn(n) {
  let h = 0;
  for (let k = 0; k < n; k++) {
    const f = new Function('a', 'b', `let s = ${k}; for (let i = 0; i < a; i++) s = (s * 31 + i + b) | 0; return s ^ ${k * 7};`);
    for (let j = 0; j < 200; j++) h = mix(h, f(40, j));
  }
  return h;
}

function collections(n) {
  const m = new Map(), set = new Set();
  for (let i = 0; i < n; i++) {
    const r = rnd();
    m.set(r % 5000, (m.get(r % 5000) || 0) + 1);
    if (r & 1) set.add(`k${r % 3000}`); else set.delete(`k${r % 3000}`);
  }
  let h = 0;
  for (const [k, v] of m) h = mix(h, k * 31 + v);
  for (const k of set) h = mix(h, k.length);
  const arr = Array.from(m.keys()).sort((a, b) => b - a);
  for (let i = 0; i < arr.length; i += 17) h = mix(h, arr[i]);
  return h;
}

async function jit(seed, scale) {
  const runs = [];
  for (let round = 0; round < 3; round++) {
    rs = seed >>> 0 || 1;
    const r = {
      shapes: shapes(2000000 * scale),
      deopts: deopts(600000 * scale),
      osr: osr(10000000 * scale),
      strings: strings(50000 * scale),
      gen: [...gen(10000 * scale)].reduce((s, v) => (s + v) % 1000000007, 0),
      async: await chain(20000 * scale),
      exceptions: exceptions(30000 * scale),
      churn: churn(1000 * scale),
      collections: collections(300000 * scale),
    };
    if (typeof gc === 'function') gc();
    runs.push(r);
  }
  for (const k of Object.keys(runs[0])) {
    for (let round = 1; round < runs.length; round++) {
      if (runs[round][k] !== runs[0][k]) fail(`jit ${k} round ${round}: ${runs[round][k]}, round 0: ${runs[0][k]}`);
    }
    emit(k, hex(runs[0][k]));
  }
}

// ---------------------------------------------------------------------------------------------
// workersToken: four worker_threads pass a numbered token through a SharedArrayBuffer. The owner
// of turn t waits (Atomics.wait) until the token is t, checks the payload the previous owner
// wrote, does JIT and allocation work, writes the next payload, advances the token with
// compareExchange and notifies. Midway, a worker spinning in a Wasm loop and one blocked in
// Atomics.wait are terminated, and a new ring of workers continues the turns.

const { Worker, isMainThread, workerData, parentPort } = require('worker_threads');

// Shared layout (Int32Array): 0 token, 1 payload, 2 payload check, 3 spinning flag, 4 errors.
const TOKEN = 0, PAYLOAD = 1, CHECK = 2, SPIN = 3, ERRORS = 4;
const payloadFor = (seed, t) => mix(mix(seed | 0, t), t * 7919) | 0;
const checkFor = (seed, t) => mix(payloadFor(seed, t), 0x5bd1e995) | 0;

// JIT noise: polymorphic property access, closures and short-lived objects.
function noise(seed, t) {
  const objs = [{ a: 1, b: 2 }, { b: 3, a: 4 }, { a: 5, c: 6, b: 7 }, [8, 9], new Map([[1, 2]])];
  let h = seed ^ t;
  for (let i = 0; i < 400; i++) {
    const o = objs[(i + t) % objs.length];
    h = mix(h, (o.a | 0) + (o.b | 0) + (o.length | 0) + (o.size | 0));
    const f = (x) => x * (i | 1) + t;
    h = mix(h, f(i) | 0);
    if (i % 50 === 0) h = mix(h, JSON.stringify({ i, t }).length);
  }
  return h;
}

function workerMain() {
  const { mode, sab, k, ring, first, last, seed } = workerData;
  const a = new Int32Array(sab);
  if (mode === 'spin') {
    // An infinite loop in Wasm: (func (loop br 0)).
    const bytes = Uint8Array.from([0, 97, 115, 109, 1, 0, 0, 0, 1, 4, 1, 96, 0, 0, 3, 2, 1, 0, 7, 8, 1, 4, 115, 112,
      105, 110, 0, 0, 10, 9, 1, 7, 0, 3, 64, 12, 0, 11, 11]);
    const spin = new WebAssembly.Instance(new WebAssembly.Module(bytes)).exports.spin;
    Atomics.store(a, SPIN, 1);
    Atomics.notify(a, SPIN);
    spin();
    return;
  }
  if (mode === 'block') {
    Atomics.store(a, SPIN, 2);
    Atomics.notify(a, SPIN);
    Atomics.wait(a, SPIN, 2); // nobody wakes it; terminate must
    return;
  }
  let h = 0, turns = 0;
  for (let t = first + k; t < last; t += ring) {
    for (;;) {
      const v = Atomics.load(a, TOKEN);
      if (v === t) break;
      if (v > t) { Atomics.add(a, ERRORS, 1); break; }
      Atomics.wait(a, TOKEN, v, 60000);
    }
    if (Atomics.load(a, PAYLOAD) !== payloadFor(seed, t) || Atomics.load(a, CHECK) !== checkFor(seed, t)) Atomics.add(a, ERRORS, 1);
    h = mix(h, noise(seed, t));
    Atomics.store(a, PAYLOAD, payloadFor(seed, t + 1));
    Atomics.store(a, CHECK, checkFor(seed, t + 1));
    if (Atomics.compareExchange(a, TOKEN, t, t + 1) !== t) Atomics.add(a, ERRORS, 1);
    Atomics.notify(a, TOKEN);
    turns++;
  }
  parentPort.postMessage({ h, turns });
}

function startWorker(data) {
  const w = new Worker(__filename, { workerData: data, argv: ['worker'] });
  const exited = new Promise((resolve) => w.on('exit', resolve));
  const message = new Promise((resolve, reject) => { w.on('message', resolve); w.on('error', reject); });
  return { w, exited, message };
}

async function workersToken(seed, scale) {
  const sab = new SharedArrayBuffer(64);
  const a = new Int32Array(sab);
  const turnsPerRing = 1500 * scale;
  Atomics.store(a, TOKEN, 0);
  Atomics.store(a, PAYLOAD, payloadFor(seed, 0));
  Atomics.store(a, CHECK, checkFor(seed, 0));
  const results = [];
  const runRing = async (base, first, last) => {
    // Worker base+k takes turns first+k, first+k+4, ...
    const ws = [0, 1, 2, 3].map((k) => startWorker({ mode: 'ring', sab, k, ring: 4, first, last, seed }));
    const msgs = await Promise.all(ws.map((x) => x.message));
    const codes = await Promise.all(ws.map((x) => x.exited));
    msgs.forEach((m, k) => results.push(`${base + k}:${m.turns}:${hex(m.h)}`));
    return codes;
  };
  const codes1 = await runRing(0, 0, turnsPerRing);
  emit('ring1.exits', codes1.join(','));
  emit('ring1.token', Atomics.load(a, TOKEN));

  // Terminate a worker spinning in Wasm and one blocked in Atomics.wait.
  const terms = [];
  for (const [mode, flag] of [['spin', 1], ['block', 2]]) {
    Atomics.store(a, SPIN, 0);
    const x = startWorker({ mode, sab, seed });
    while (Atomics.load(a, SPIN) !== flag) Atomics.wait(a, SPIN, 0, 1000);
    const code = await x.w.terminate();
    terms.push(`${mode}:${code}:${await x.exited}`);
  }
  emit('terminated', terms.join(','));

  // A replacement ring continues the token from where the first stopped.
  const codes2 = await runRing(4, turnsPerRing, 2 * turnsPerRing);
  emit('ring2.exits', codes2.join(','));
  emit('ring2.token', Atomics.load(a, TOKEN));
  emit('payload', hex(Atomics.load(a, PAYLOAD)));
  emit('errors', Atomics.load(a, ERRORS));
  let h = 0;
  for (const r of results) for (let i = 0; i < r.length; i++) h = mix(h, r.charCodeAt(i));
  emit('workers', hex(h));
  if (Atomics.load(a, ERRORS) !== 0) fail(`workersToken: ${Atomics.load(a, ERRORS)} errors`);
  if (Atomics.load(a, TOKEN) !== 2 * turnsPerRing) fail('workersToken: token did not reach the end');
}

async function main() {
  const [test, seedArg, scaleArg] = process.argv.slice(2);
  const seed = Number(seedArg), scale = Number(scaleArg);
  if (!Number.isInteger(seed) || !Number.isInteger(scale) || scale < 1) fail('usage: jsfz wasm|jit|workersToken SEED SCALE');
  rs = seed >>> 0 || 1;
  if (test === 'wasm') wasm(seed, scale);
  else if (test === 'jit') await jit(seed, scale);
  else if (test === 'workersToken') await workersToken(seed, scale);
  else fail(`unknown test ${test}`);
  console.log(`OK Js.${test} ${hex(digest)}`);
}
if (isMainThread) main().catch((e) => fail(String(e && e.stack || e).split('\n')[0]));
else workerMain();

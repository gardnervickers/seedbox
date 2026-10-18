-- LuaJIT probes: trace recording, side exits, side traces and stitching, with hot thresholds
-- lowered so traces form, exit and link constantly; jit.flush() discarding all machine code
-- while loops keep running; and a tiny machine-code area that refills over freed code. Every
-- result is checked against the interpreter (jit.off).
--
-- Usage: luafz trace|ffi SEED SCALE
local test, seed, scale = arg[1], tonumber(arg[2]), tonumber(arg[3])
local bit = require("bit")
local ffi = require("ffi")
local band, bxor, lshift, rshift, tobit = bit.band, bit.bxor, bit.lshift, bit.rshift, bit.tobit

-- FNV-1a step on 32-bit values: h * 16777619 written as exact shifts and adds.
local function mix(h, x)
  h = bxor(h, tobit(x))
  return tobit(lshift(h, 24) + lshift(h, 8) + lshift(h, 7) + lshift(h, 4) + lshift(h, 1) + h)
end

local digest = tobit(0x811c9dc5)
local function emit(k, v)
  local line = k .. " " .. tostring(v)
  io.write(line, "\n")
  for i = 1, #line do digest = mix(digest, line:byte(i)) end
end
local function fail(msg)
  io.write("FAIL ", msg, "\n")
  io.stdout:flush()
  os.exit(1)
end
local function hex(h) return bit.tohex(h) end

-- The per-iteration hook: in the flushing mode it throws away every trace now and then.
local flushing = false
local function tick(i)
  if flushing and i % 997 == 0 then jit.flush() end
end

-- ---------------------------------------------------------------------------------------------
-- trace

local function branchy(n, k)
  local s = 0
  for i = 1, n do
    if i % k == 0 then s = s - i elseif i % 3 == 1 then s = s + 2 * i else s = s + i end
    tick(i)
  end
  return s
end

-- Values of changing types at one site: every type guard has a side exit and side trace.
local function poly(vals, n)
  local acc = 0
  for i = 1, n do
    local v = vals[i % #vals + 1]
    local t = type(v)
    if t == "number" then acc = acc + v
    elseif t == "string" then acc = acc + #v
    elseif t == "table" then acc = acc + (v.x or 0) + #v
    elseif t == "boolean" then acc = acc + (v and 1 or 2)
    else acc = acc + 3 end
    acc = acc % 1000000007
    tick(i)
  end
  return acc
end

local function fib(n) if n < 2 then return n end return fib(n - 1) + fib(n - 2) end

local function closures(n)
  local fs = {}
  for k = 1, 16 do
    local c = k
    fs[k] = function(x) c = (c * 7 + x) % 65521 return c end
  end
  local s = 0
  for i = 1, n do
    s = (s + fs[i % 16 + 1](i)) % 1000000007
    tick(i)
  end
  return s
end

-- String fast functions, some compiled and some stitched around.
local function strings(n, r)
  local parts, h = {}, 0
  for i = 1, n do
    r = (r * 1103515245 + 12345) % 2147483648
    local s = string.format("%d:%s", r % 1000, string.char(97 + r % 26, 97 + math.floor(r / 26) % 26))
    parts[#parts + 1] = s
    h = mix(h, s:byte(1) + #s * 256)
    if i % 50 == 0 then
      local joined = table.concat(parts, ",")
      local count = 0
      for a, b in joined:gmatch("(%d+):(%a+)") do count = count + #a + #b end
      h = mix(h, count)
      h = mix(h, #joined:gsub("%d", "") + joined:find(":", 1, true))
      parts = {}
    end
    tick(i)
  end
  return h
end

local function tables(n, r)
  local t, arr = {}, {}
  for i = 1, n do
    r = (r * 1103515245 + 12345) % 2147483648
    local k = r % 3000
    t["k" .. k] = (t["k" .. k] or 0) + 1
    if r % 4 == 0 then arr[#arr + 1] = k elseif #arr > 0 and r % 4 == 1 then table.remove(arr) end
    tick(i)
  end
  local keys = {}
  for k, v in pairs(t) do keys[#keys + 1] = k end
  table.sort(keys)
  local h = #arr
  for i = 1, #keys do h = mix(h, #keys[i] * 31 + t[keys[i]]) end
  table.sort(arr, function(a, b) return a > b end)
  for i = 1, #arr, 7 do h = mix(h, arr[i]) end
  return h
end

local function floats(n)
  local x, s = 0.5, 0
  for i = 1, n do
    x = x * 1.0000001 + math.sqrt(i % 100) * 1e-3
    if x > 1e3 then x = x - 1e3 end
    s = s + math.floor(x * 1024) % 4096
    tick(i)
  end
  return s
end

local function trace_suite(sd, sc)
  local vals = { 1, 2.5, "abc", { x = 3, 1, 2 }, true, false, 7, "z", {}, 4 }
  return {
    branchy = branchy(600000 * sc, 7),
    poly = poly(vals, 600000 * sc),
    fib = fib(24),
    closures = closures(600000 * sc),
    strings = strings(60000 * sc, sd),
    tables = tables(300000 * sc, sd),
    floats = floats(600000 * sc),
  }
end

-- ---------------------------------------------------------------------------------------------
-- ffi

ffi.cdef [[
typedef struct { double x, y; int32_t tag; uint8_t flags[3]; } fz_pt;
void qsort(void *base, size_t nmemb, size_t size, int (*compar)(const void *, const void *));
size_t strlen(const char *s);
int memcmp(const void *a, const void *b, size_t n);
]]

local function ffi_suite(sd, sc)
  local n = 100000 * sc
  local a = ffi.new("int32_t[?]", n)
  local r = sd
  for i = 0, n - 1 do
    r = (r * 1103515245 + 12345) % 2147483648
    a[i] = r - 1073741824
  end
  local s64 = 0LL
  for i = 0, n - 1 do s64 = s64 + a[i] * 3LL; tick(i) end
  local u = 0xfedcba9876543210ULL
  for i = 1, 50000 * sc do u = u * 6364136223846793005ULL + 1442695040888963407ULL; tick(i) end

  local pts = ffi.new("fz_pt[?]", 1000)
  local acc = 0
  for i = 0, 500000 * sc - 1 do
    local p = pts[i % 1000]
    p.x = p.x + i % 13
    p.y = p.y * 0.5 + p.x
    p.tag = bxor(p.tag, i)
    p.flags[i % 3] = p.flags[i % 3] + 1
    acc = (acc + p.tag + p.flags[0]) % 1000000007
    tick(i)
  end
  local ph = 0
  for i = 0, 999 do ph = mix(ph, math.floor(pts[i].y) + pts[i].tag + pts[i].flags[1]) end

  -- qsort calls back into Lua through a machine-code callback slot.
  local copy = ffi.new("int32_t[?]", 4000)
  ffi.copy(copy, a, 4000 * 4)
  local cb = ffi.cast("int (*)(const void *, const void *)", function(p, q)
    local x, y = ffi.cast("const int32_t *", p)[0], ffi.cast("const int32_t *", q)[0]
    return x < y and -1 or (x > y and 1 or 0)
  end)
  ffi.C.qsort(copy, 4000, 4, cb)
  cb:free()
  local sh = 0
  for i = 1, 3999 do
    if copy[i - 1] > copy[i] then fail("qsort order at " .. i) end
    if i % 13 == 0 then sh = mix(sh, copy[i]) end
  end

  local buf = ffi.new("char[64]")
  local lh = 0
  for i = 1, 10000 * sc do
    ffi.fill(buf, 64, 0)
    ffi.copy(buf, "seed" .. (i * 7919 % 100000))
    lh = mix(lh, tonumber(ffi.C.strlen(buf)) + ffi.C.memcmp(buf, "seed1", 5))
    lh = mix(lh, #ffi.string(buf, 6))
    tick(i)
  end
  return {
    sum64 = tostring(s64), lcg64 = tostring(u), points = hex(ph) .. "/" .. acc,
    qsort = hex(sh), cstrings = hex(lh),
  }
end

-- ---------------------------------------------------------------------------------------------

local MODES = {
  { "interp", function() jit.off(); jit.flush() end },
  { "jit", function() jit.on() end },
  -- Traces start and side traces attach after one hot loop or exit; tick flushes them.
  { "hot", function() jit.on(); jit.opt.start("hotloop=1", "hotexit=1", "tryside=1") end, true },
  -- A machine-code area of a few kilobytes fills up and is flushed and reused repeatedly.
  { "tinymcode", function() jit.on(); jit.opt.start("hotloop=2", "hotexit=2", "sizemcode=4", "maxmcode=16") end },
}

if (test ~= "trace" and test ~= "ffi") or not seed or not scale or scale < 1 then
  fail("usage: luafz trace|ffi SEED SCALE")
end
local suite = test == "trace" and trace_suite or ffi_suite
local ref
for _, m in ipairs(MODES) do
  m[2]()
  flushing = m[3] or false
  local res = suite(seed % 2147483647 + 1, scale)
  jit.flush()
  if not ref then
    ref = res
  else
    for k, v in pairs(ref) do
      if res[k] ~= v then fail(string.format("%s mode %s: %s, interpreter %s", k, m[1], tostring(res[k]), tostring(v))) end
    end
  end
end
local keys = {}
for k in pairs(ref) do keys[#keys + 1] = k end
table.sort(keys)
for _, k in ipairs(keys) do emit(k, ref[k]) end
emit("status", (select(1, jit.status())) and "jit" or "off")
io.write("OK Lua.", test, " ", hex(digest), "\n")

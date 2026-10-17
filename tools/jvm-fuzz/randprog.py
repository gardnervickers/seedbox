"""Random deterministic Java programs for differential JIT and translator testing.

Each program is a single class whose output depends only on its seed. It mixes
integer, long and double arithmetic, guarded and unguarded array accesses,
caught arithmetic and bounds exceptions, virtual calls, monitors and string
building. ``main`` runs the entry function through increasing phases so the
same code executes interpreted, then compiled by C1, then by C2 (including
on-stack replacement), printing a checksum after every phase.
"""

import random

INT_EDGES = ["0", "1", "-1", "2", "7", "31", "32", "63", "64", "0x7fffffff", "0x80000000", "0xffff", "-2147483647"]
LONG_EDGES = ["0L", "1L", "-1L", "63L", "64L", "0x7fffffffffffffffL", "0x8000000000000000L", "0xffffffffL", "0x100000000L"]
DOUBLE_EDGES = ["0.0", "-0.0", "1.0", "-1.5", "0.1", "1e300", "-1e-300", "Double.NaN", "Double.POSITIVE_INFINITY",
                "Double.MIN_VALUE", "3.141592653589793"]
INT_FIELDS = ["sI0", "sI1", "sI2", "sI3"]
LONG_FIELDS = ["sL0", "sL1", "sL2"]
DOUBLE_FIELDS = ["sD0", "sD1"]


class Var:
    def __init__(self, name, typ, writable=True):
        self.name, self.typ, self.writable = name, typ, writable


class Gen:
    def __init__(self, seed, name):
        self.r = random.Random(seed)
        self.name = name
        self.counter = 0
        self.nfuncs = self.r.randint(3, 6)
        self.costs = {}
        self.in_method = False

    def fresh(self, prefix):
        self.counter += 1
        return f"{prefix}{self.counter}"

    # Expressions -----------------------------------------------------------

    def leaf(self, typ, scope):
        r = self.r
        vars_ = [v for v in scope if v.typ == typ]
        choice = r.random()
        if typ == "int":
            if choice < 0.35 and vars_:
                return r.choice(vars_).name
            if choice < 0.5:
                return r.choice(INT_FIELDS)
            if choice < 0.6:
                return f"A[({self.expr('int', 0, scope)}) & 63]"
            return r.choice(INT_EDGES) if r.random() < 0.4 else str(r.randint(-1000, 1000))
        if typ == "long":
            if choice < 0.35 and vars_:
                return r.choice(vars_).name
            if choice < 0.5:
                return r.choice(LONG_FIELDS)
            if choice < 0.6:
                return f"L[({self.expr('int', 0, scope)}) & 31]"
            return r.choice(LONG_EDGES) if r.random() < 0.4 else f"{r.randint(-10**12, 10**12)}L"
        if typ == "double":
            if choice < 0.4 and vars_:
                return r.choice(vars_).name
            if choice < 0.55:
                return r.choice(DOUBLE_FIELDS)
            return r.choice(DOUBLE_EDGES) if r.random() < 0.4 else repr(r.uniform(-100, 100))
        if typ == "boolean":
            if vars_ and choice < 0.3:
                return r.choice(vars_).name
            return r.choice(["true", "false"])
        raise ValueError(typ)

    def expr(self, typ, depth, scope):
        r = self.r
        if depth <= 0 or r.random() < 0.25:
            return self.leaf(typ, scope)
        d = depth - 1
        e = lambda t=typ: self.expr(t, d, scope)
        if typ == "int":
            k = r.randint(0, 15)
            if k <= 3:
                return f"({e()} {r.choice(['+', '-', '*', '&', '|', '^'])} {e()})"
            if k == 4:
                return f"({e()} {r.choice(['<<', '>>', '>>>'])} {e()})"
            if k == 5:
                return f"({e()} {r.choice(['/', '%'])} ({e()} | 1))"
            if k == 6:
                return f"{r.choice(['-', '~'])}({e()})"
            if k == 7:
                return f"(int) ({e('long')})"
            if k == 8:
                return f"(int) ({e('double')})"
            if k == 9:
                return f"({e('boolean')} ? {e()} : {e()})"
            if k == 10:
                f = r.choice(["Integer.bitCount", "Integer.numberOfLeadingZeros", "Integer.numberOfTrailingZeros",
                              "Integer.reverseBytes", "Integer.reverse", "Math.abs", "Integer.highestOneBit", "Integer.signum"])
                return f"{f}({e()})"
            if k == 11:
                f = r.choice(["Integer.rotateLeft", "Integer.rotateRight", "Math.max", "Math.min", "Integer.compare",
                              "Integer.compareUnsigned", "Integer.compress", "Integer.expand"])
                return f"{f}({e()}, {e()})"
            if k == 12 and not self.in_method:
                return f"OBJS[({e()}) & 7].m({e()})"
            if k == 13:
                return f"({r.choice(['(byte)', '(short)', '(char)'])} {e()})"
            if k == 14:
                return f"mixI({e()}, {e()})"
            return f"({e()} {r.choice(['/', '%'])} {r.choice(['3', '7', '10', '-5', '1000', '16'])})"
        if typ == "long":
            k = r.randint(0, 11)
            if k <= 3:
                return f"({e()} {r.choice(['+', '-', '*', '&', '|', '^'])} {e()})"
            if k == 4:
                return f"({e()} {r.choice(['<<', '>>', '>>>'])} {e('int')})"
            if k == 5:
                return f"({e()} {r.choice(['/', '%'])} ({e()} | 1L))"
            if k == 6:
                return f"(long) ({e('int')})"
            if k == 7:
                return f"(long) ({e('double')})"
            if k == 8:
                f = r.choice(["Math.multiplyHigh", "Math.unsignedMultiplyHigh", "Math.max", "Math.min", "Long.rotateLeft"])
                second = e("int") if f == "Long.rotateLeft" else e()
                return f"{f}({e()}, {second})"
            if k == 9:
                f = r.choice(["Long.bitCount", "Long.numberOfLeadingZeros", "Long.numberOfTrailingZeros"])
                return f"(long) {f}({e()})"
            if k == 10:
                return f"({e('boolean')} ? {e()} : {e()})"
            return f"({e()} / {r.choice(['3L', '10L', '1000000007L', '-7L'])})"
        if typ == "double":
            k = r.randint(0, 8)
            if k <= 3:
                return f"({e()} {r.choice(['+', '-', '*', '/', '%'])} {e()})"
            if k == 4:
                return f"(double) ({e('int')})"
            if k == 5:
                return f"(double) ({e('long')})"
            if k == 6:
                f = r.choice(["Math.sqrt", "Math.abs", "Math.floor", "Math.ceil", "Math.rint", "Math.sin", "Math.cos",
                              "Math.exp", "Math.log", "Math.cbrt", "Math.signum"])
                return f"{f}({e()})"
            if k == 7:
                return f"Math.fma({e()}, {e()}, {e()})"
            return f"(float) ({e()})"
        if typ == "boolean":
            k = r.randint(0, 5)
            if k <= 1:
                t = r.choice(["int", "long", "double"])
                return f"({self.expr(t, d, scope)} {r.choice(['<', '<=', '>', '>=', '==', '!='])} {self.expr(t, d, scope)})"
            if k == 2:
                return f"(({e('int')} & {1 << r.randint(0, 5)}) == 0)"
            if k == 3:
                return f"!{e()}"
            if k == 4:
                return f"({e()} {r.choice(['&&', '||', '^'])} {e()})"
            return f"Double.isNaN({e('double')})"
        raise ValueError(typ)

    # Statements ------------------------------------------------------------

    def block(self, n, depth, scope, loops, fidx, out, indent):
        scope = list(scope)
        cost = 0
        for _ in range(n):
            cost += self.stmt(depth, scope, loops, fidx, out, indent)
        return cost

    def target(self, scope, typ=None):
        cands = [v for v in scope if v.writable and (typ is None or v.typ == typ) and v.typ != "boolean"]
        fields = [Var(f, "int") for f in INT_FIELDS] + [Var(f, "long") for f in LONG_FIELDS] + [Var(f, "double") for f in DOUBLE_FIELDS]
        fields = [f for f in fields if typ is None or f.typ == typ]
        if cands and self.r.random() < 0.75:
            return self.r.choice(cands)
        return self.r.choice(fields)

    def stmt(self, depth, scope, loops, fidx, out, indent):
        r = self.r
        pad = "    " * indent
        ed = r.randint(1, 3)
        k = r.random()
        if depth <= 0 or k < 0.30:
            kind = r.random()
            if kind < 0.3:
                typ = r.choice(["int", "int", "long", "double", "boolean"])
                name = self.fresh("v")
                out.append(f"{pad}{typ} {name} = {self.expr(typ, ed, scope)};")
                scope.append(Var(name, typ))
            elif kind < 0.8:
                v = self.target(scope)
                op = r.choice(["=", "+=", "^=", "-="]) if v.typ != "double" else r.choice(["=", "+=", "*="])
                out.append(f"{pad}{v.name} {op} {self.expr(v.typ, ed, scope)};")
            else:
                if r.random() < 0.5:
                    out.append(f"{pad}A[({self.expr('int', 1, scope)}) & 63] {r.choice(['=', '+=', '^='])} {self.expr('int', ed, scope)};")
                else:
                    out.append(f"{pad}L[({self.expr('int', 1, scope)}) & 31] {r.choice(['=', '+='])} {self.expr('long', ed, scope)};")
            return 1
        if k < 0.45:
            out.append(f"{pad}if ({self.expr('boolean', 2, scope)}) {{")
            c1 = self.block(r.randint(1, 3), depth - 1, scope, loops, fidx, out, indent + 1)
            out.append(f"{pad}}} else {{")
            c2 = self.block(r.randint(0, 2), depth - 1, scope, loops, fidx, out, indent + 1)
            out.append(f"{pad}}}")
            return 1 + max(c1, c2)
        if k < 0.60 and loops < 2:
            trip = r.randint(1, 12) if loops else r.randint(4, 40)
            iv = self.fresh("i")
            if r.random() < 0.7:
                out.append(f"{pad}for (int {iv} = 0; {iv} < {trip}; {iv}++) {{")
            else:
                out.append(f"{pad}int {iv} = {trip};")
                out.append(f"{pad}while ({iv}-- > 0) {{")
            inner = scope + [Var(iv, "int", writable=False)]
            c = self.block(r.randint(1, 4), depth - 1, inner, loops + 1, fidx, out, indent + 1)
            out.append(f"{pad}}}")
            return 1 + trip * c
        if k < 0.68:
            out.append(f"{pad}switch (({self.expr('int', 2, scope)}) & 7) {{")
            worst = 0
            for case in sorted(r.sample(range(8), r.randint(1, 4))):
                out.append(f"{pad}    case {case}: {{")
                worst = max(worst, self.block(r.randint(1, 2), depth - 1, scope, loops, fidx, out, indent + 2))
                out.append(f"{pad}        break;")
                out.append(f"{pad}    }}")
            out.append(f"{pad}    default: {{")
            worst = max(worst, self.block(1, depth - 1, scope, loops, fidx, out, indent + 2))
            out.append(f"{pad}    }}")
            out.append(f"{pad}}}")
            return 1 + worst
        if k < 0.78:
            v = self.target(scope, "int")
            risky = r.choice([
                f"{v.name} {r.choice(['+=', '^='])} {self.expr('int', 2, scope)} / {self.expr('int', 1, scope)};",
                f"{v.name} {r.choice(['+=', '='])} A[{self.expr('int', 2, scope)} % 80];",
                f"A[{self.expr('int', 1, scope)} % 70] = {self.expr('int', 2, scope)};",
                f"{v.name} += (int) (L[(int) ({self.expr('long', 1, scope)} % 40L)] % {self.expr('long', 1, scope)});",
            ])
            out.append(f"{pad}try {{")
            out.append(f"{pad}    {risky}")
            c = self.block(r.randint(0, 2), depth - 1, scope, loops, fidx, out, indent + 1)
            out.append(f"{pad}}} catch (ArithmeticException ex) {{")
            out.append(f"{pad}    {v.name} += {r.randint(1, 99)};")
            out.append(f"{pad}}} catch (ArrayIndexOutOfBoundsException ex) {{")
            out.append(f"{pad}    {v.name} ^= {r.randint(1, 99)};")
            out.append(f"{pad}}}")
            return 3 + c
        if k < 0.86 and fidx + 1 < self.nfuncs and loops < 2:
            callee = r.randint(fidx + 1, self.nfuncs - 1)
            v = self.target(scope, r.choice(["int", "long"]))
            cast = "(int) " if v.typ == "int" else ""
            out.append(f"{pad}{v.name} += {cast}f{callee}({self.expr('int', 2, scope)}, {self.expr('long', 2, scope)});")
            return 1 + self.costs.get(callee, 50)
        if k < 0.91:
            out.append(f"{pad}synchronized (LOCK) {{")
            c = self.block(r.randint(1, 2), depth - 1, scope, loops, fidx, out, indent + 1)
            out.append(f"{pad}}}")
            return 2 + c
        if k < 0.95:
            v = self.target(scope, "int")
            out.append(f"{pad}SB.setLength(0);")
            out.append(f"{pad}SB.append({self.expr('int', 1, scope)}).append('/').append({self.expr('long', 1, scope)}).append({self.expr('double', 1, scope)});")
            out.append(f"{pad}{v.name} += SB.toString().hashCode();")
            return 4
        v = self.target(scope, "long")
        out.append(f"{pad}{v.name} += {self.expr('long', 3, scope)};")
        return 1

    # Program ---------------------------------------------------------------

    def function(self, idx):
        params = [Var("p0", "int"), Var("p1", "long")]
        body = []
        cost = self.block(self.r.randint(4, 10), 4, params, 0, idx, body, 2)
        locals_ = []
        for line in body:
            s = line.strip()
            for typ in ("int ", "long ", "double "):
                if s.startswith(typ) and "=" in s and not s.startswith(typ + "i"):
                    name = s[len(typ):].split("=")[0].strip()
                    if line.startswith("        ") and not line.startswith("         "):
                        locals_.append((name, typ.strip()))
        ret = ["(long) p0", "p1"]
        for name, typ in locals_:
            ret.append(f"Double.doubleToLongBits({name})" if typ == "double" else f"(long) {name}")
        src = [f"    static long f{idx}(int p0, long p1) {{"] + body
        src.append(f"        return {' * 31 + '.join(ret)};")
        src.append("    }")
        self.costs[idx] = cost
        return src

    def program(self):
        r = self.r
        funcs = []
        for idx in reversed(range(self.nfuncs)):
            funcs = self.function(idx) + [""] + funcs
        subclasses = []
        self.in_method = True
        for k in range(4):
            scope = [Var("x", "int")]
            subclasses.append(f"    static final class B{k} extends B {{ int m(int x) {{ return {self.expr('int', 3, scope)}; }} }}")
        self.in_method = False
        objs = ", ".join(f"new B{r.randint(0, 3)}()" for _ in range(8))
        cost = max(1, self.costs[0])
        iterations = max(20_000, min(400_000, BUDGET // cost))
        phases = [1, 10, 100, 2_000, iterations // 4, iterations]
        lines = [
            "// Generated by tools/jvm-fuzz/randprog.py; output depends only on the seed.",
            f"public final class {self.name} {{",
            "    static int sI0 = 1, sI1 = -7, sI2 = 0x12345678, sI3;",
            "    static long sL0 = 3L, sL1 = -1L, sL2 = 0x0123456789abcdefL;",
            "    static double sD0 = 0.5, sD1 = -2.25;",
            "    static final int[] A = new int[64];",
            "    static final long[] L = new long[32];",
            "    static final Object LOCK = new Object();",
            "    static final StringBuilder SB = new StringBuilder();",
            "    static abstract class B { abstract int m(int x); }",
            *subclasses,
            f"    static final B[] OBJS = {{{objs}}};",
            "",
            "    static int mixI(int a, int b) { return Integer.rotateLeft(a * 0x9E3779B9, 7) ^ b; }",
            "",
            *funcs,
            "    static long state() {",
            "        long h = sI0 * 31L + sI1 + sI2 * 7L + sI3 + sL0 + sL1 * 3 + sL2 + Double.doubleToLongBits(sD0) + Double.doubleToLongBits(sD1);",
            "        for (int x : A) h = h * 31 + x;",
            "        for (long x : L) h = h * 31 + x;",
            "        return h;",
            "    }",
            "",
            "    public static void main(String[] args) {",
            "        long salt = args.length > 0 ? Long.parseLong(args[0]) : 0;",
            f"        int[] phases = {{{', '.join(map(str, phases))}}};",
            "        long check = 0;",
            "        int done = 0;",
            "        for (int ph = 0; ph < phases.length; ph++) {",
            "            for (; done < phases[ph]; done++) check = check * 31 + f0(done, check ^ salt);",
            "            System.out.println(\"phase\" + ph + \" \" + Long.toHexString(check) + \" \" + Long.toHexString(state()));",
            "        }",
            f"        System.out.println(\"OK {self.name} \" + Long.toHexString(check ^ state()));",
            "    }",
            "}",
        ]
        return "\n".join(lines) + "\n"


BUDGET = 30_000_000
MAX_COST = BUDGET // 20_000


def generate(seed, name):
    """Returns a program for ``seed``, retrying sub-seeds until its estimated run cost fits the budget."""
    for attempt in range(1000):
        g = Gen(seed * 1000 + attempt, name)
        src = g.program()
        if g.costs[0] <= MAX_COST:
            return src
    raise RuntimeError(f"no affordable program for seed {seed}")


if __name__ == "__main__":
    import sys
    print(generate(int(sys.argv[1]), sys.argv[2] if len(sys.argv) > 2 else "R0"))

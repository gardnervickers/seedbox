// CoreCLR probes: tiered compilation (tier 0, call counting, tier 1 with PGO, on-stack
// replacement) through interface stub dispatch, shared generics and hardware-fault exceptions;
// and the generational, compacting collector with card-marking write barriers while other
// threads run and must be suspended.
//
// Usage: csfz tier|gc SEED SCALE
using System.Numerics;
using System.Runtime.CompilerServices;
using System.Runtime.InteropServices;
using System.Runtime.Intrinsics;
using System.Text;

static class Fz
{
    static uint digest = 2166136261;

    public static void Emit(string key, object value)
    {
        string line = $"{key} {value}";
        Console.WriteLine(line);
        foreach (char c in line + "\n") digest = (digest ^ c) * 16777619;
    }

    public static void Fail(string msg)
    {
        Console.WriteLine("FAIL " + msg);
        Environment.Exit(1);
    }

    public static ulong Mix(ulong h, ulong x)
    {
        h = (h ^ x) * 0x100000001b3UL;
        return h ^ (h >> 29);
    }

    public static ulong Next(ref ulong x)
    {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        return x;
    }

    static int Main(string[] args)
    {
        if (args.Length != 3 || !ulong.TryParse(args[1], out ulong seed) || !int.TryParse(args[2], out int scale) || scale < 1)
        {
            Console.WriteLine("FAIL usage: csfz tier|gc SEED SCALE");
            return 2;
        }
        switch (args[0])
        {
            case "tier": Tier.Run(seed | 1, scale); break;
            case "gc": Gc.Run(seed | 1, scale); break;
            default: Fail("unknown test " + args[0]); break;
        }
        Console.WriteLine($"OK Cs.{args[0]} {digest:x8}");
        return 0;
    }
}

interface IOp { long Apply(long x); }
sealed class AddOp : IOp { readonly long k; public AddOp(long k) { this.k = k; } public long Apply(long x) => x + k; }
sealed class MulOp : IOp { readonly long k; public MulOp(long k) { this.k = k; } public long Apply(long x) => (x * k) | 1; }
sealed class XorOp : IOp { readonly int k; public XorOp(int k) { this.k = k; } public long Apply(long x) => x ^ (x >> k); }
sealed class RotOp : IOp { readonly int k; public RotOp(int k) { this.k = k; } public long Apply(long x) => (long)BitOperations.RotateLeft((ulong)x, k); }
struct NotOp : IOp { public long Apply(long x) => ~x; }
sealed class DivOp : IOp { readonly long k; public DivOp(long k) { this.k = k; } public long Apply(long x) => x / k + x % k + 0x1234567; }

abstract class Animal
{
    public abstract int Legs(int x);
    public virtual int Weight(int x) => x;
}
sealed class Dog : Animal { public override int Legs(int x) => x + 4; }
sealed class Bird : Animal { public override int Legs(int x) => x + 2; public override int Weight(int x) => x / 2; }
class Snake : Animal { public override int Legs(int x) => x; public override int Weight(int x) => x * 3; }
sealed class Python : Snake { public override int Weight(int x) => base.Weight(x) + 7; }

static class Tier
{
    // Generic math over value types: one instantiation per type.
    static T Sum<T>(ReadOnlySpan<T> s) where T : INumber<T>
    {
        T acc = T.Zero;
        foreach (var v in s) acc += v;
        return acc;
    }

    // Shared canonical code for reference types, with runtime generic lookups.
    static int Count<T>(List<T> l, Func<T, bool> pred) where T : class
    {
        int n = 0;
        foreach (var v in l) if (pred(v)) n++;
        return n;
    }

    // A null string dereference is a SIGSEGV that the runtime turns into an exception.
    [MethodImpl(MethodImplOptions.NoInlining)]
    static int Faults(int i, string s, int[] arr, object o)
    {
        int r = 0;
        try { r += s.Length; } catch (NullReferenceException) { r += 1000; }
        try { r += arr[i]; } catch (IndexOutOfRangeException) { r += 2000; }
        try { r += 100 / (i % 5); } catch (DivideByZeroException) { r += 3000; }
        try { r += ((string)o).Length; } catch (InvalidCastException) { r += 4000; }
        try { r += checked(int.MaxValue - 10 + i % 20); } catch (OverflowException) { r += 5000; }
        return r;
    }

    // Called once per phase: its loops start in tier-0 code and reach optimized code by OSR,
    // while its callees are promoted by call counting.
    static ulong Kernel(ulong seed, int n)
    {
        ulong x = seed, h = 14695981039346656037UL;
        IOp[] ops = { new AddOp(3), new MulOp(5), new XorOp(7), new RotOp(11), new NotOp(), new DivOp(9) };
        Animal[] zoo = { new Dog(), new Bird(), new Snake(), new Python() };
        long acc = 1;
        for (int i = 0; i < n; i++)
        {
            ulong r = Fz.Next(ref x);
            acc = ops[(int)(r % 6)].Apply(acc);
            var a = zoo[(int)((r >> 8) % 4)];
            acc += a.Legs((int)(r >> 16) & 0xff) + a.Weight((int)(r >> 24) & 0xff);
            if ((r & 0xff) == 0) h = Fz.Mix(h, (ulong)acc);
        }
        h = Fz.Mix(h, (ulong)acc);

        Span<int> ints = stackalloc int[256];
        Span<double> doubles = stackalloc double[256];
        long[] longs = new long[256];
        for (int i = 0; i < 256; i++)
        {
            ulong r = Fz.Next(ref x);
            ints[i] = (int)(r % 100000);
            doubles[i] = (r % 1000) / 8.0;
            longs[i] = (long)(r >> 3);
        }
        h = Fz.Mix(h, (ulong)Sum<int>(ints));
        h = Fz.Mix(h, (ulong)(long)(Sum<double>(doubles) * 8));
        h = Fz.Mix(h, (ulong)Sum<long>(longs));

        var strs = new List<string>();
        for (int i = 0; i < 2000; i++) strs.Add((Fz.Next(ref x) % 5000).ToString());
        h = Fz.Mix(h, (ulong)Count(strs, s => s.Length == 3));
        h = Fz.Mix(h, (ulong)Enumerable.Range(0, 5000).Select(v => v * 3).Where(v => v % 7 != 0).Sum());
        var dict = new Dictionary<string, int>();
        foreach (var s in strs) dict[s] = dict.TryGetValue(s, out int c) ? c + 1 : 1;
        h = Fz.Mix(h, (ulong)dict.Count);
        foreach (var kv in dict.OrderBy(kv => kv.Key, StringComparer.Ordinal).Take(50)) h = Fz.Mix(h, (ulong)(kv.Key.Length * 31 + kv.Value));

        var va = Vector128.Create((int)x, 2, 3, 4);
        var vb = Vector128.Create(5, 6, 7, (int)(x >> 32));
        for (int i = 0; i < 5000; i++) va = Vector128.Xor(va + vb, Vector128.ShiftLeft(va, 1));
        h = Fz.Mix(h, (uint)Vector128.Sum(va));

        int[] arr = new int[8];
        for (int i = 0; i < 300; i++)
            h = Fz.Mix(h, (ulong)Faults(i % 16, (i & 3) == 0 ? null : "abc", arr, (i & 1) == 0 ? "s" : (object)i));

        var sb = new StringBuilder();
        for (int i = 0; i < 3000; i++) sb.Append((char)('a' + (int)(Fz.Next(ref x) % 26))).Append(i % 10);
        foreach (char c in sb.ToString()) h = Fz.Mix(h, c);
        return h;
    }

    // A short method called many times: tier 0, then tier 1 once call counting finishes.
    [MethodImpl(MethodImplOptions.NoInlining)]
    static ulong Small(ulong x)
    {
        ulong h = x;
        for (int i = 0; i < 64; i++) h = Fz.Mix(h, (ulong)i * x);
        return h;
    }

    public static void Run(ulong seed, int scale)
    {
        int n = 1_000_000 * scale;
        ulong first = Kernel(seed, n);
        ulong small = 0;
        for (int phase = 0; phase < 6; phase++)
        {
            ulong s = 0;
            for (int i = 0; i < 3000; i++) s = Fz.Mix(s, Small(seed + (ulong)i));
            if (phase == 0) small = s;
            else if (s != small) Fz.Fail($"tier small phase {phase}: {s:x16}, phase 0: {small:x16}");
            Thread.Sleep(150); // the tiering delay expires and tier-1 code is installed
            ulong again = Kernel(seed, n);
            if (again != first) Fz.Fail($"tier kernel phase {phase}: {again:x16}, first: {first:x16}");
        }
        Fz.Emit("kernel", first.ToString("x16"));
        Fz.Emit("small", small.ToString("x16"));

        // The same kernel on three threads at once, against a sequential run.
        var want = new ulong[3];
        for (int t = 0; t < 3; t++) want[t] = Kernel(seed + (ulong)t * 2, n / 2);
        var got = new ulong[3];
        var threads = new Thread[3];
        for (int t = 0; t < 3; t++)
        {
            int k = t;
            threads[t] = new Thread(() => got[k] = Kernel(seed + (ulong)k * 2, n / 2));
            threads[t].Start();
        }
        foreach (var th in threads) th.Join();
        for (int t = 0; t < 3; t++) if (got[t] != want[t]) Fz.Fail($"tier thread {t}: {got[t]:x16}, want {want[t]:x16}");
        ulong h = 0;
        foreach (var v in got) h = Fz.Mix(h, v);
        Fz.Emit("threads", h.ToString("x16"));
    }
}

sealed class Node
{
    public Node Next;
    public Node[] Kids;
    public long Val;
    public byte[] Blob;
}

sealed class Finalized
{
    public static int Count;
    public long Pad1, Pad2;
    ~Finalized() { Interlocked.Increment(ref Count); }
}

static class Gc
{
    // A seeded mutation sequence on a graph promoted to the oldest generation, so every young
    // node stored into an old one goes through the card-marking write barrier.
    static ulong Mutate(ulong seed, int ops, bool collect)
    {
        ulong x = seed;
        var old = new Node[4096];
        for (int i = 0; i < old.Length; i++) old[i] = new Node { Val = i, Kids = new Node[4] };
        if (collect)
        {
            GC.Collect();
            GC.Collect();
        }
        for (int i = 0; i < ops; i++)
        {
            ulong r = Fz.Next(ref x);
            var o = old[(int)(r % 4096)];
            switch ((r >> 12) % 8)
            {
                case 0: case 1: case 2:
                    o.Kids[(r >> 16) % 4] = new Node { Val = (long)r, Next = o.Kids[(r >> 18) % 4] };
                    break;
                case 3: o.Next = old[(int)((r >> 20) % 4096)]; break;
                case 4: o.Kids[(r >> 16) % 4] = null; break;
                case 5:
                    {
                        var b = old[(int)(r % 256)];
                        b.Blob = new byte[(r >> 24) % 32 == 0 ? 85_000 + (int)((r >> 30) % 40_000) : 1 + (int)((r >> 30) % 2000)];
                        b.Blob[0] = (byte)r;
                        b.Blob[^1] = (byte)(r >> 8);
                        break;
                    }
                case 6: o.Val = o.Val * 31 + (long)(r & 0xffff); break;
                case 7:
                    if (collect && (r >> 40) % 1024 == 0)
                    {
                        if ((r >> 50) % 4 == 0) System.Runtime.GCSettings.LargeObjectHeapCompactionMode = System.Runtime.GCLargeObjectHeapCompactionMode.CompactOnce;
                        GC.Collect((int)((r >> 52) % 3), GCCollectionMode.Forced, true, ((r >> 55) & 1) == 1);
                    }
                    break;
            }
        }
        ulong h = 0;
        foreach (var o in old)
        {
            h = Fz.Mix(h, (ulong)o.Val);
            h = Fz.Mix(h, (ulong)(o.Next?.Val ?? -1));
            foreach (var k in o.Kids)
            {
                int d = 0;
                for (var c = k; c != null && d < 8; c = c.Next, d++) h = Fz.Mix(h, (ulong)c.Val);
                h = Fz.Mix(h, (ulong)d);
            }
            if (o.Blob != null) h = Fz.Mix(h, (ulong)o.Blob.Length << 16 | (ulong)o.Blob[0] << 8 | o.Blob[^1]);
        }
        return h;
    }

    [MethodImpl(MethodImplOptions.NoInlining)]
    static WeakReference[] Garbage(int n)
    {
        var weak = new WeakReference[n];
        for (int i = 0; i < n; i++)
        {
            new Finalized { Pad1 = i };
            weak[i] = new WeakReference(new Node { Val = i });
        }
        return weak;
    }

    public static unsafe void Run(ulong seed, int scale)
    {
        int ops = 1_000_000 * scale;
        var want = new ulong[3];
        for (int t = 0; t < 3; t++) want[t] = Mutate(seed + (ulong)t, ops, false);

        // Three mutators while this thread keeps forcing compacting collections.
        var got = new ulong[3];
        var threads = new Thread[3];
        for (int t = 0; t < 3; t++)
        {
            int k = t;
            threads[t] = new Thread(() => got[k] = Mutate(seed + (ulong)k, ops, true));
            threads[t].Start();
        }
        byte[] pinned = GC.AllocateArray<byte>(4096, pinned: true);
        var handleTarget = new byte[4096];
        var handle = GCHandle.Alloc(handleTarget, GCHandleType.Pinned);
        int rounds = 0;
        while (threads.Any(th => th.IsAlive))
        {
            fixed (byte* p = pinned)
            {
                byte* q = (byte*)handle.AddrOfPinnedObject();
                for (int i = 0; i < 4096; i++) { p[i] = (byte)(i + rounds); q[i] = (byte)(i ^ rounds); }
                GC.Collect(2, GCCollectionMode.Forced, true, true);
                for (int i = 0; i < 4096; i++)
                    if (p[i] != (byte)(i + rounds) || handleTarget[i] != (byte)(i ^ rounds)) Fz.Fail($"gc pinned byte {i} changed");
            }
            rounds++;
            Thread.Sleep(2);
        }
        handle.Free();
        foreach (var th in threads) th.Join();
        for (int t = 0; t < 3; t++) if (got[t] != want[t]) Fz.Fail($"gc mutator {t}: {got[t]:x16}, want {want[t]:x16}");
        ulong h = 0;
        foreach (var v in got) h = Fz.Mix(h, v);
        Fz.Emit("mutate", h.ToString("x16"));

        int n = 2000 * scale;
        var weak = Garbage(n);
        GC.Collect();
        GC.WaitForPendingFinalizers();
        GC.Collect();
        Fz.Emit("finalized", Finalized.Count);
        Fz.Emit("weak_alive", weak.Count(w => w.IsAlive));
    }
}

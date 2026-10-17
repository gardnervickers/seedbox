import java.lang.invoke.CallSite;
import java.lang.invoke.LambdaMetafactory;
import java.lang.invoke.MethodHandle;
import java.lang.invoke.MethodHandles;
import java.lang.invoke.MethodType;
import java.lang.invoke.MutableCallSite;
import java.lang.invoke.SwitchPoint;
import java.util.function.IntBinaryOperator;
import java.util.function.LongUnaryOperator;
import java.util.function.Supplier;

/** invokedynamic: lambdas, string concatenation, pattern switches, method-handle combinators and relinked call sites. */
public final class Indy {
    sealed interface Expr permits Num, Add, Mul, Neg {}
    record Num(long v) implements Expr {}
    record Add(Expr l, Expr r) implements Expr {}
    record Mul(Expr l, Expr r) implements Expr {}
    record Neg(Expr e) implements Expr {}
    enum Color { RED, GREEN, BLUE }

    static long eval(Expr e) {
        return switch (e) {
            case Num n -> n.v();
            case Add(Expr l, Expr r) -> eval(l) + eval(r);
            case Mul(Expr l, Expr r) when eval(l) == 0 -> 0;
            case Mul(Expr l, Expr r) -> eval(l) * eval(r);
            case Neg(Expr inner) -> -eval(inner);
        };
    }

    static Expr gen(int d) {
        if (d == 0) return new Num(Fz.rnd.nextInt(-9, 10));
        return switch (Fz.rnd.nextInt(4)) {
            case 0 -> new Add(gen(d - 1), gen(d - 1));
            case 1 -> new Mul(gen(d - 1), gen(d - 1));
            case 2 -> new Neg(gen(d - 1));
            default -> new Num(Fz.rnd.nextInt(100));
        };
    }

    static String describe(Object o) {
        return switch (o) {
            case null -> "null";
            case Integer i when i > 100 -> "big" + i;
            case Integer i -> "int" + i;
            case String s -> "str" + s.length();
            case Color c -> "color" + c.ordinal();
            case int[] a -> "arr" + a.length;
            default -> o.getClass().getSimpleName();
        };
    }

    static int plus(int a, int b) { return a + b; }
    static int times(int a, int b) { return a * b; }
    static boolean isEven(int a, int b) { return (a & 1) == 0; }
    static int fails(int a, int b) { if (b % 7 == 0) throw new IllegalStateException(); return a - b; }
    static int recover(IllegalStateException e, int a, int b) { return -1; }

    public static void main(String[] args) throws Throwable {
        Fz.init("Indy", args);
        Expr e = gen(12);
        long ev = 0;
        for (int i = 0; i < 2_000 * Fz.scale; i++) ev += eval(e);
        Fz.out("eval", ev);
        Object[] samples = {null, 5, 500, "hello", Color.BLUE, new int[3], 2.5, new Num(1)};
        StringBuilder sb = new StringBuilder();
        for (Object o : samples) sb.append(describe(o)).append(',');
        Fz.out("describe", sb);
        long concat = 0;
        for (int i = 0; i < 100_000; i++) {
            String s = "a" + i + 'c' + (i * 2L) + 1.5f + true + null + Color.values()[i % 3] + (char) ('A' + i % 26);
            concat += s.hashCode();
        }
        Fz.out("concat", concat);
        long lam = 0;
        for (int i = 0; i < 100_000; i++) {
            final int k = i;
            LongUnaryOperator f = x -> x * k + 1;
            Supplier<String> s = () -> "v" + k;
            IntBinaryOperator g = Math::max;
            lam += f.applyAsLong(3) + s.get().length() + g.applyAsInt(k, 50_000);
        }
        Fz.out("lambda", lam);
        MethodHandles.Lookup lk = MethodHandles.lookup();
        MethodType ii = MethodType.methodType(int.class, int.class, int.class);
        MethodHandle plus = lk.findStatic(Indy.class, "plus", ii);
        MethodHandle times = lk.findStatic(Indy.class, "times", ii);
        MethodHandle test = lk.findStatic(Indy.class, "isEven", MethodType.methodType(boolean.class, int.class, int.class));
        MethodHandle gwt = MethodHandles.guardWithTest(test, plus, times);
        MethodHandle guarded = MethodHandles.catchException(lk.findStatic(Indy.class, "fails", ii), IllegalStateException.class,
            lk.findStatic(Indy.class, "recover", MethodType.methodType(int.class, IllegalStateException.class, int.class, int.class)));
        MethodHandle filtered = MethodHandles.filterArguments(gwt, 0, MethodHandles.identity(int.class));
        MutableCallSite site = new MutableCallSite(ii);
        site.setTarget(plus);
        MethodHandle dyn = site.dynamicInvoker();
        SwitchPoint sp = new SwitchPoint();
        MethodHandle sw = sp.guardWithTest(plus, times);
        long mh = 0;
        for (int i = 0; i < 300_000; i++) {
            mh += (int) filtered.invokeExact(i, 3);
            mh += (int) guarded.invokeExact(i, i % 11);
            mh += (int) dyn.invokeExact(i, 2);
            mh += (int) sw.invokeExact(i, 2);
            if (i == 150_000) {
                site.setTarget(times);
                MutableCallSite.syncAll(new MutableCallSite[] {site});
                SwitchPoint.invalidateAll(new SwitchPoint[] {sp});
            }
        }
        Fz.out("methodHandles", mh);
        CallSite cs = LambdaMetafactory.metafactory(lk, "applyAsInt", MethodType.methodType(IntBinaryOperator.class), ii, plus, ii);
        IntBinaryOperator op = (IntBinaryOperator) cs.getTarget().invokeExact();
        Fz.out("metafactory", op.applyAsInt(40, 2));
        Fz.done();
    }
}

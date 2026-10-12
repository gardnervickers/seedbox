import java.lang.annotation.Retention;
import java.lang.annotation.RetentionPolicy;
import java.lang.reflect.Constructor;
import java.lang.reflect.Field;
import java.lang.reflect.InvocationHandler;
import java.lang.reflect.Method;
import java.lang.reflect.Proxy;
import java.lang.reflect.RecordComponent;
import java.util.Arrays;
import java.util.Comparator;

/** Core reflection: invocation inflation, fields, constructors, dynamic proxies, annotations and records. */
public final class Reflection {
    @Retention(RetentionPolicy.RUNTIME)
    @interface Tag { String value(); int weight() default 1; }

    interface Calc { long apply(long a, long b); String name(); }

    record Point(int x, long y, String label) {}

    static final class Target {
        private int hidden = 7;
        public static long sfield = 3;
        @Tag(value = "add", weight = 2) public long add(long a, long b) { return a + b; }
        @Tag("mul") public long mul(long a, long b) { return a * b; }
        private String secret(String s) { return s + hidden; }
        Target() {}
        Target(int h) { hidden = h; }
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Reflection", args);
        Target t = new Target();
        Method add = Target.class.getMethod("add", long.class, long.class);
        Method mul = Target.class.getMethod("mul", long.class, long.class);
        Method secret = Target.class.getDeclaredMethod("secret", String.class);
        secret.setAccessible(true);
        long acc = 0;
        for (int i = 0; i < 50_000 * Fz.scale; i++) {
            acc += (long) add.invoke(t, (long) i, 3L);
            acc ^= (long) mul.invoke(t, (long) i, 5L);
            if (i % 1000 == 0) Fz.mix((String) secret.invoke(t, "s" + i));
        }
        Fz.out("invoke", acc);
        Field hidden = Target.class.getDeclaredField("hidden");
        hidden.setAccessible(true);
        Field sfield = Target.class.getField("sfield");
        for (int i = 0; i < 20_000; i++) {
            hidden.setInt(t, hidden.getInt(t) + 1);
            sfield.setLong(null, sfield.getLong(null) * 3 + 1);
        }
        Fz.out("fields", hidden.getInt(t) + " " + sfield.getLong(null));
        Constructor<Target> ctor = Target.class.getDeclaredConstructor(int.class);
        long cs = 0;
        for (int i = 0; i < 10_000; i++) cs += hidden.getInt(ctor.newInstance(i));
        Fz.out("ctor", cs);
        Method[] ms = Target.class.getDeclaredMethods();
        Arrays.sort(ms, Comparator.comparing(Method::getName));
        StringBuilder sb = new StringBuilder();
        for (Method m : ms) {
            Tag tag = m.getAnnotation(Tag.class);
            sb.append(m.getName()).append(tag == null ? "" : "@" + tag.value() + tag.weight()).append(' ');
        }
        Fz.out("methods", sb.toString().trim());
        InvocationHandler h = (proxy, m, a) -> switch (m.getName()) {
            case "apply" -> (Long) a[0] * 31 + (Long) a[1];
            case "name" -> "proxy";
            case "hashCode" -> 42;
            case "equals" -> proxy == a[0];
            default -> m.getName();
        };
        Calc c = (Calc) Proxy.newProxyInstance(Reflection.class.getClassLoader(), new Class<?>[] {Calc.class}, h);
        long ps = 0;
        for (int i = 0; i < 100_000; i++) ps += c.apply(i, ps & 0xff);
        Fz.out("proxy", ps + " " + c.name() + " " + c.hashCode() + " " + c.toString());
        Point p = new Point(3, 4L, "p");
        StringBuilder rc = new StringBuilder();
        for (RecordComponent comp : Point.class.getRecordComponents()) rc.append(comp.getName()).append('=').append(comp.getAccessor().invoke(p)).append(' ');
        Fz.out("record", rc.toString().trim() + " " + p);
        Fz.out("forName", Class.forName("java.util.concurrent.ConcurrentSkipListMap").getSimpleName() + " " + int[][].class.getName());
        Fz.done();
    }
}

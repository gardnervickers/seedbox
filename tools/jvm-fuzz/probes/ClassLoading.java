import java.io.InputStream;
import java.lang.invoke.MethodHandles;
import java.lang.ref.WeakReference;
import java.util.ArrayList;
import java.util.List;
import java.util.function.LongUnaryOperator;

/** Repeated class definition in fresh and hidden loaders, JIT of each copy, then unloading and code-cache reuse. */
public final class ClassLoading {
    static final class OneShot extends ClassLoader {
        final byte[] bytes;
        OneShot(byte[] bytes) { super(ClassLoading.class.getClassLoader()); this.bytes = bytes; }
        protected Class<?> loadClass(String name, boolean resolve) throws ClassNotFoundException {
            if (name.equals("Payload")) {
                synchronized (getClassLoadingLock(name)) {
                    Class<?> c = findLoadedClass(name);
                    if (c == null) c = defineClass(name, bytes, 0, bytes.length);
                    return c;
                }
            }
            return super.loadClass(name, resolve);
        }
    }

    public static void main(String[] args) throws Exception {
        Fz.init("ClassLoading", args);
        byte[] bytes;
        try (InputStream in = ClassLoading.class.getResourceAsStream("/Payload.class")) {
            bytes = in.readAllBytes();
        }
        int rounds = 4 * Fz.scale;
        int perRound = 40;
        List<WeakReference<Class<?>>> dead = new ArrayList<>();
        long acc = 0;
        for (int r = 0; r < rounds; r++) {
            List<LongUnaryOperator> live = new ArrayList<>();
            for (int i = 0; i < perRound; i++) {
                Class<?> c;
                if ((i & 1) == 0) {
                    c = new OneShot(bytes).loadClass("Payload");
                } else {
                    c = MethodHandles.lookup().defineHiddenClass(bytes, false).lookupClass();
                }
                dead.add(new WeakReference<>(c));
                live.add((LongUnaryOperator) c.getDeclaredConstructor().newInstance());
            }
            for (int it = 0; it < 30_000; it++) {
                for (LongUnaryOperator op : live) acc += op.applyAsLong(it);
            }
            Fz.mix(acc);
            live.clear();
            System.gc();
        }
        int unloaded = 0;
        for (int g = 0; g < 10; g++) {
            System.gc();
            unloaded = 0;
            for (WeakReference<Class<?>> w : dead) if (w.get() == null) unloaded++;
            if (unloaded == dead.size()) break;
        }
        Fz.out("acc", acc);
        Fz.out("classes", dead.size());
        Fz.out("allUnloaded", unloaded == dead.size());
        Fz.done();
    }
}

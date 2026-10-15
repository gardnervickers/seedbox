import java.io.ByteArrayOutputStream;
import java.io.InputStream;
import java.nio.charset.StandardCharsets;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.List;
import java.util.function.IntUnaryOperator;
import java.util.function.Supplier;
import java.util.jar.Attributes;
import java.util.jar.JarEntry;
import java.util.jar.JarOutputStream;
import java.util.jar.Manifest;
import java.util.stream.Collectors;
import java.util.stream.IntStream;

/**
 * Dynamic application class-data sharing in child JVMs: one run writes a dynamic archive at exit
 * (-XX:ArchiveClassesAtExit), later runs map it with -Xshare:on and must load the application and
 * its lambda proxies from the archive's top layer and compute the same result; the JDK 19
 * auto-created archive does the same in two runs. Only exit codes, results and whether classes
 * came from the archive are printed.
 */
public final class Cds {
    /** The archived application: lambdas, a stream pipeline and indified string concatenation. */
    public static final class App {
        public static void main(String[] args) {
            long seed = Long.parseLong(args[0]);
            IntUnaryOperator f = x -> x + 1;
            Supplier<String> s = () -> "seed" + seed;
            String joined = IntStream.range(0, 200).map(f).map(x -> (int) (x * seed % 1000))
                .mapToObj(Integer::toString).collect(Collectors.joining(","));
            List<Runnable> rs = new ArrayList<>();
            long[] acc = {seed};
            for (int i = 0; i < 50; i++) {
                final int k = i;
                rs.add(() -> acc[0] = acc[0] * 31 + k);
            }
            rs.forEach(Runnable::run);
            System.out.println("app " + f.applyAsInt(41) + " " + s.get() + " " + joined.hashCode() + " " + acc[0]);
        }
    }

    static Path jar(Path dir) throws Exception {
        Path jar = dir.resolve("app.jar");
        Manifest m = new Manifest();
        m.getMainAttributes().put(Attributes.Name.MANIFEST_VERSION, "1.0");
        try (JarOutputStream out = new JarOutputStream(Files.newOutputStream(jar), m);
             InputStream in = Cds.class.getResourceAsStream("/Cds$App.class")) {
            out.putNextEntry(new JarEntry("Cds$App.class"));
            out.write(in.readAllBytes());
            out.closeEntry();
        }
        return jar;
    }

    record Run(int exit, List<String> lines) {}

    static Run java(Path dir, Path jar, String... flags) throws Exception {
        List<String> cmd = new ArrayList<>();
        cmd.add(Path.of(System.getProperty("java.home"), "bin", "java").toString());
        cmd.addAll(List.of("-Xmx128m", "-Xss1m", "-XX:-UsePerfData", "-XX:TieredStopAtLevel=1"));
        cmd.addAll(List.of(flags));
        cmd.addAll(List.of("-cp", jar.toString(), "Cds$App", Long.toString(Fz.seed)));
        Process p = new ProcessBuilder(cmd).directory(dir.toFile()).redirectErrorStream(true)
            .redirectInput(ProcessBuilder.Redirect.from(new java.io.File("/dev/null"))).start();
        ByteArrayOutputStream out = new ByteArrayOutputStream();
        p.getInputStream().transferTo(out);
        int code = p.waitFor();
        return new Run(code, out.toString(StandardCharsets.UTF_8).lines().toList());
    }

    static String app(Run r) {
        return r.lines.stream().filter(l -> l.startsWith("app ")).findFirst().orElse("missing");
    }

    /** Whether the class-loading log shows `name` (and only names starting with it) loaded from the top layer. */
    static boolean fromTop(Run r, String name) {
        return r.lines.stream().anyMatch(l -> l.contains(" " + name) && l.contains("source: shared objects file (top)"));
    }

    static void report(String key, Run r) {
        Fz.out(key, "exit=" + r.exit + " " + app(r));
        if (r.exit != 0 || app(r).equals("missing")) {
            for (String l : r.lines) System.out.println("| " + l);
        }
        Fz.mix(app(r));
    }

    public static void main(String[] args) throws Exception {
        Fz.init("Cds", args);
        Path dir = Files.createTempDirectory("fzcds");
        Path jar = jar(dir);
        Path archive = dir.resolve("app.jsa");
        Run dump = java(dir, jar, "-XX:ArchiveClassesAtExit=" + archive, "-Xlog:cds=off", "-Xlog:cds+dynamic=off");
        report("dump", dump);
        Fz.out("archiveWritten", Files.exists(archive) && Files.size(archive) > 4096);
        String load = "-Xlog:class+load=info";
        Run use = java(dir, jar, "-Xshare:on", "-XX:SharedArchiveFile=" + archive, load);
        report("use", use);
        Fz.out("useFromTop", fromTop(use, "Cds$App ") + " lambda=" + fromTop(use, "Cds$App$$Lambda"));
        // Interpreted only, so the archived classes run without any compiled code.
        Run useInt = java(dir, jar, "-Xshare:on", "-XX:SharedArchiveFile=" + archive, "-Xint");
        report("useInt", useInt);
        Path auto = dir.resolve("auto.jsa");
        Run create = java(dir, jar, "-XX:+AutoCreateSharedArchive", "-XX:SharedArchiveFile=" + auto, "-Xlog:cds=off");
        report("autoCreate", create);
        Run reuse = java(dir, jar, "-XX:+AutoCreateSharedArchive", "-XX:SharedArchiveFile=" + auto, load);
        report("autoReuse", reuse);
        Fz.out("autoFromTop", fromTop(reuse, "Cds$App "));
        String want = app(dump);
        Fz.expect(dump.exit == 0 && use.exit == 0 && useInt.exit == 0 && create.exit == 0 && reuse.exit == 0, "child exits");
        Fz.expect(want.startsWith("app 42 ") && want.equals(app(use)) && want.equals(app(useInt))
            && want.equals(app(create)) && want.equals(app(reuse)), "same result with and without the archive");
        Fz.expect(fromTop(use, "Cds$App "), "application class loaded from the dynamic archive");
        try (var files = Files.list(dir)) {
            for (Path p : files.toList()) Files.delete(p);
        }
        Files.delete(dir);
        Fz.done();
    }
}

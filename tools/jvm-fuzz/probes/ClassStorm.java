import java.net.URI;
import java.nio.file.FileSystem;
import java.nio.file.FileSystems;
import java.nio.file.Files;
import java.nio.file.Path;
import java.util.ArrayList;
import java.util.Collections;
import java.util.List;

/**
 * Loads thousands of JDK classes without initializing them, read from the mapped jrt image:
 * file-backed page faults, class parsing, metaspace growth. Prints the count and a hash of the
 * sorted names that loaded.
 */
public final class ClassStorm {
    public static void main(String[] args) throws Exception {
        Fz.init("ClassStorm", args);
        FileSystem jrt = FileSystems.getFileSystem(URI.create("jrt:/"));
        List<String> names = new ArrayList<>();
        for (String module : new String[] {"java.base", "java.desktop", "java.xml", "java.sql", "java.management"}) {
            Path root = jrt.getPath("/modules", module);
            try (var walk = Files.walk(root)) {
                walk.filter(p -> p.toString().endsWith(".class") && !p.toString().endsWith("module-info.class"))
                    .forEach(p -> names.add(root.relativize(p).toString().replace('/', '.').replaceAll("\\.class$", "")));
            }
        }
        Collections.sort(names);
        int loaded = 0;
        long h = 0;
        ClassLoader loader = ClassLoader.getPlatformClassLoader();
        for (String n : names) {
            try {
                Class.forName(n, false, loader);
                loaded++;
                h = h * 31 + n.hashCode();
            } catch (Throwable t) {
                h = h * 31 + 1;
            }
        }
        Fz.out("candidates", names.size());
        Fz.out("loaded", loaded);
        Fz.outHex("names", h);
        Fz.done();
    }
}

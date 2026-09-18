import java.io.InputStream;
import java.nio.file.Path;
import java.util.Arrays;
import java.util.Map;
import java.util.concurrent.ConcurrentSkipListMap;
import java.util.jar.JarFile;
import java.util.jar.JarOutputStream;
import java.util.zip.ZipEntry;
import net.fabricmc.tinyremapper.TinyRemapper;
import net.fabricmc.tinyremapper.TinyUtils;
import net.fabricmc.tinyremapper.extension.mixin.MixinExtension;

public final class RemapModToStdout {
    private RemapModToStdout() { }

    public static void main(String[] args) throws Exception {
        Path input = Path.of(args[0]);
        Path mappings = Path.of(args[1]);
        String from = args[2];
        String to = args[3];
        Map<String, byte[]> classes = new ConcurrentSkipListMap<>();

        TinyRemapper remapper = TinyRemapper.newRemapper()
                .withMappings(TinyUtils.createTinyMappingProvider(mappings, from, to))
                .propagatePrivate(true)
                .propagateBridges(TinyRemapper.LinkedMethodPropagation.ENABLED)
                .ignoreConflicts(true)
                .extension(new MixinExtension())
                .build();

        try {
            for (int i = 4; i < args.length; i++) {
                remapper.readClassPath(Path.of(args[i]));
            }

            remapper.readInputs(input);
            remapper.apply((name, bytes) -> {
                String entryName = name + ".class";
                byte[] previous = classes.put(entryName, bytes);
                if (previous != null && !Arrays.equals(previous, bytes)) {
                    throw new IllegalStateException("conflicting remapped class " + name);
                }
            });
        } finally {
            remapper.finish();
        }

        try (JarFile source = new JarFile(input.toFile());
                JarOutputStream output = new JarOutputStream(System.out)) {
            source.stream().sequential().forEach(entry -> {
                String name = entry.getName();
                if (name.endsWith(".class") || isDetachedSignature(name)) return;

                try (InputStream stream = source.getInputStream(entry)) {
                    ZipEntry out = new ZipEntry(name);
                    out.setTime(entry.getTime());
                    output.putNextEntry(out);
                    stream.transferTo(output);
                    output.closeEntry();
                } catch (Exception e) {
                    throw new IllegalStateException("cannot copy resource " + name, e);
                }
            });

            classes.forEach((name, bytes) -> {
                try {
                    output.putNextEntry(new ZipEntry(name));
                    output.write(bytes);
                    output.closeEntry();
                } catch (Exception e) {
                    throw new IllegalStateException("cannot write class " + name, e);
                }
            });
        }

        System.err.println("remapped classes=" + classes.size());
    }

    private static boolean isDetachedSignature(String name) {
        if (!name.startsWith("META-INF/") || name.indexOf('/', 9) >= 0) return false;
        String file = name.substring(9);
        return file.endsWith(".SF") || file.endsWith(".DSA") || file.endsWith(".RSA")
                || file.endsWith(".EC") || file.startsWith("SIG-");
    }
}

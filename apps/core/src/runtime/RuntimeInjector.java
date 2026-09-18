package jvmsense.runtime;

import java.lang.reflect.Method;
import java.security.CodeSource;
import java.util.ArrayList;
import java.util.HashSet;
import java.util.List;
import java.util.Set;
import java.util.concurrent.atomic.AtomicInteger;

import org.objectweb.asm.ClassReader;
import org.objectweb.asm.ClassWriter;
import org.objectweb.asm.Opcodes;
import org.objectweb.asm.tree.AbstractInsnNode;
import org.objectweb.asm.tree.ClassNode;
import org.objectweb.asm.tree.MethodInsnNode;
import org.objectweb.asm.tree.MethodNode;

/** Runtime-only bytecode preparation for JVMTI class redefinition. */
public final class RuntimeInjector {
    private static final AtomicInteger HANDLER_IDS = new AtomicInteger();

    private RuntimeInjector() {
    }

    /**
     * Move methods introduced by Mixin into a generated static-handler class
     * and redirect call sites in the target class.
     */
    public static byte[] prepareRedefinition(
            ClassLoader classLoader,
            String targetName,
            byte[] beforeBytes,
            byte[] afterBytes) throws Exception {
        ClassNode before = read(beforeBytes);
        ClassNode after = read(afterBytes);
        Set<String> beforeMethods = new HashSet<>();
        for (MethodNode method : before.methods) {
            beforeMethods.add(method.name + method.desc);
        }

        List<MethodNode> moved = new ArrayList<>();
        for (MethodNode method : after.methods) {
            if (!beforeMethods.contains(method.name + method.desc)) {
                moved.add(method);
            }
        }
        if (moved.isEmpty()) {
            return afterBytes;
        }

        String targetInternal = targetName.replace('.', '/');
        String generatedInternal = generatedName(targetInternal);
        byte[] handlerBytes = generateHandlers(
                targetInternal,
                generatedInternal,
                after.version,
                moved);
        define(classLoader, generatedInternal.replace('/', '.'), handlerBytes);

        Set<String> movedKeys = new HashSet<>();
        for (MethodNode method : moved) {
            movedKeys.add(method.name + method.desc);
        }
        for (MethodNode method : after.methods) {
            for (AbstractInsnNode instruction : method.instructions.toArray()) {
                if (!(instruction instanceof MethodInsnNode)) {
                    continue;
                }
                MethodInsnNode call = (MethodInsnNode) instruction;
                if (!call.owner.equals(targetInternal)) {
                    continue;
                }
                if (!movedKeys.contains(call.name + call.desc)) {
                    continue;
                }
                MethodNode movedMethod = findMoved(moved, call.name);
                if (movedMethod == null) {
                    continue;
                }
                call.setOpcode(Opcodes.INVOKESTATIC);
                call.owner = generatedInternal;
                if ((movedMethod.access & Opcodes.ACC_STATIC) == 0) {
                    call.desc = "(L" + targetInternal + ";" + call.desc.substring(1);
                }
            }
        }

        after.methods.removeAll(moved);
        ClassWriter writer = new ClassWriter(0);
        after.accept(writer);
        return writer.toByteArray();
    }

    private static MethodNode findMoved(List<MethodNode> moved, String name) {
        for (MethodNode method : moved) {
            if (method.name.equals(name)) {
                return method;
            }
        }
        return null;
    }

    private static ClassNode read(byte[] bytes) {
        ClassReader reader = new ClassReader(bytes);
        ClassNode node = new ClassNode();
        reader.accept(node, 0);
        return node;
    }

    private static String generatedName(String targetInternal) {
        int slash = targetInternal.lastIndexOf('/');
        String prefix = slash < 0 ? "" : targetInternal.substring(0, slash + 1);
        return prefix + "JvmsenseStaticHandlers_" + HANDLER_IDS.incrementAndGet();
    }

    private static byte[] generateHandlers(
            String targetInternal,
            String generatedInternal,
            int version,
            List<MethodNode> methods) {
        ClassWriter writer = new ClassWriter(0);
        writer.visit(version, Opcodes.ACC_PUBLIC | Opcodes.ACC_SUPER,
                generatedInternal, null, "java/lang/Object", null);
        for (MethodNode method : methods) {
            boolean isStatic = (method.access & Opcodes.ACC_STATIC) != 0;
            int access = (method.access & ~(Opcodes.ACC_PRIVATE | Opcodes.ACC_PROTECTED))
                    | Opcodes.ACC_PUBLIC | Opcodes.ACC_STATIC;
            String descriptor = isStatic
                    ? method.desc
                    : "(L" + targetInternal + ";" + method.desc.substring(1);
            MethodNode copy = new MethodNode(access, method.name, descriptor,
                    method.signature, method.exceptions.toArray(new String[0]));
            method.accept(copy);
            copy.accept(writer.visitMethod(access, method.name, descriptor,
                    method.signature, method.exceptions.toArray(new String[0])));
        }
        writer.visitEnd();
        return writer.toByteArray();
    }

    private static void define(ClassLoader loader, String binaryName, byte[] bytes) throws Exception {
        Method define = loader.getClass().getDeclaredMethod(
                "defineClassFwd",
                String.class,
                byte[].class,
                int.class,
                int.class,
                CodeSource.class);
        define.setAccessible(true);
        define.invoke(loader, binaryName, bytes, 0, bytes.length, null);
    }
}

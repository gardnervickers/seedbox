import java.io.*;
import java.util.ArrayList;
import java.util.HashMap;
import java.util.List;
import java.util.Map;

/** Java serialization of cyclic graphs, custom hooks, externalizable classes and records. */
public final class Serialization {
    static final class Node implements Serializable {
        private static final long serialVersionUID = 1L;
        int id;
        List<Node> edges = new ArrayList<>();
        transient int scratch = 99;
        Map<String, Object> attrs = new HashMap<>();
        Node(int id) { this.id = id; }
        private void readObject(ObjectInputStream in) throws IOException, ClassNotFoundException { in.defaultReadObject(); scratch = -id; }
    }

    static final class Ext implements Externalizable {
        long a; String b;
        public Ext() {}
        Ext(long a, String b) { this.a = a; this.b = b; }
        public void writeExternal(ObjectOutput out) throws IOException { out.writeLong(a); out.writeUTF(b); }
        public void readExternal(ObjectInput in) throws IOException { a = in.readLong(); b = in.readUTF(); }
    }

    record Rec(int x, String y, double[] z) implements Serializable {}

    public static void main(String[] args) throws Exception {
        Fz.init("Serialization", args);
        int n = 500 * Fz.scale;
        List<Node> nodes = new ArrayList<>();
        for (int i = 0; i < n; i++) nodes.add(new Node(i));
        // Edges point backwards so serializing the list in order keeps recursion shallow.
        for (Node x : nodes) {
            for (int e = 0; e < 3; e++) x.edges.add(nodes.get(Fz.rnd.nextInt(x.id + 1)));
            x.attrs.put("k" + x.id % 5, x.id % 2 == 0 ? Integer.valueOf(x.id) : new Ext(x.id, "e" + x.id));
        }
        Object root = new Object[] {nodes.get(0), new Rec(7, "r", new double[] {1.5, Double.NaN}), new Ext(-1, "ünï"), nodes};
        byte[] bytes = null;
        for (int r = 0; r < 20; r++) {
            ByteArrayOutputStream bos = new ByteArrayOutputStream();
            try (ObjectOutputStream oos = new ObjectOutputStream(bos)) { oos.writeObject(root); }
            bytes = bos.toByteArray();
            Object back;
            try (ObjectInputStream ois = new ObjectInputStream(new ByteArrayInputStream(bytes))) { back = ois.readObject(); }
            Object[] arr = (Object[]) back;
            @SuppressWarnings("unchecked") List<Node> ns = (List<Node>) arr[3];
            Fz.expect(arr[0] == ns.get(0), "shared reference lost");
            long h = 0;
            for (Node x : ns) {
                h = h * 31 + x.id + x.scratch;
                for (Node e : x.edges) h = h * 7 + e.id;
            }
            if (r == 0) Fz.outHex("graph", h);
            Fz.expect(((Rec) arr[1]).y().equals("r") && ((Ext) arr[2]).b.equals("ünï"), "fields");
        }
        Fz.out("bytes", bytes.length);
        Fz.mix(bytes);
        Fz.done();
    }
}

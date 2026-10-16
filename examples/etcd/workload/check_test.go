package main

// Host-only checker unit tests. These tiny RPC doubles are not etcd fixtures
// and are never used by the guest acceptance run or as determinism evidence.
import (
	"context"
	"net"
	"strings"
	"testing"

	pb "go.etcd.io/etcd/api/v3/etcdserverpb"
	"go.etcd.io/etcd/api/v3/mvccpb"
	"google.golang.org/grpc"
)

type checkerMember struct {
	pb.UnimplementedKVServer
	pb.UnimplementedMaintenanceServer
	t      *testing.T
	values map[string]string
	hash   uint32
}

func (m *checkerMember) Status(context.Context, *pb.StatusRequest) (*pb.StatusResponse, error) {
	return &pb.StatusResponse{Header: &pb.ResponseHeader{Revision: 42}}, nil
}

func (m *checkerMember) HashKV(_ context.Context, request *pb.HashKVRequest) (*pb.HashKVResponse, error) {
	if request.Revision != 42 {
		m.t.Errorf("hash revision=%d, want settled revision 42", request.Revision)
	}
	return &pb.HashKVResponse{Header: &pb.ResponseHeader{Revision: 42}, Hash: m.hash}, nil
}

func (m *checkerMember) Range(_ context.Context, request *pb.RangeRequest) (*pb.RangeResponse, error) {
	if string(request.Key) != "k/" || request.Revision != 42 || !request.Serializable {
		m.t.Errorf("checker range does not read settled member state: %+v", request)
	}
	response := &pb.RangeResponse{Header: &pb.ResponseHeader{Revision: 42}}
	for key, value := range m.values {
		response.Kvs = append(response.Kvs, &mvccpb.KeyValue{Key: []byte(key), Value: []byte(value)})
	}
	return response, nil
}

func checkerEndpoint(t *testing.T, values map[string]string, hash uint32) string {
	t.Helper()
	listener, err := net.Listen("tcp", "127.0.0.1:0")
	if err != nil {
		t.Fatal(err)
	}
	server := grpc.NewServer()
	member := &checkerMember{t: t, values: values, hash: hash}
	pb.RegisterKVServer(server, member)
	pb.RegisterMaintenanceServer(server, member)
	done := make(chan struct{})
	go func() {
		defer close(done)
		if err := server.Serve(listener); err != nil {
			t.Errorf("checker RPC double: %v", err)
		}
	}()
	t.Cleanup(func() { server.Stop(); <-done })
	return listener.Addr().String()
}

func TestCheckerRequiresEveryAckOnEveryMemberAndMatchingHashes(t *testing.T) {
	for _, defect := range []string{"none", "missing", "wrong-value", "hash-mismatch", "planted-ack"} {
		t.Run(defect, func(t *testing.T) {
			acks := []ack{{key: "k/00/000000", val: "first", rev: 40}, {key: "k/00/000001", val: "second", rev: 42}}
			var endpoints []string
			for member := 0; member < 3; member++ {
				values := map[string]string{"k/00/000000": "first", "k/00/000001": "second"}
				hash := uint32(17)
				if member == 2 {
					switch defect {
					case "missing":
						delete(values, "k/00/000000")
					case "wrong-value":
						values["k/00/000001"] = "wrong"
					case "hash-mismatch":
						hash = 18
					}
				}
				endpoints = append(endpoints, checkerEndpoint(t, values, hash))
			}
			if defect == "planted-ack" {
				acks = append(acks, ack{key: "k/99/000000", val: "never-written", rev: 42})
			}
			ok, detail := check(endpoints, acks)
			if defect == "none" {
				if !ok || !strings.Contains(detail, "rev=42 hash=11") {
					t.Fatalf("valid: ok=%v detail=%s", ok, detail)
				}
			} else {
				if ok {
					t.Fatalf("checker accepted %s: %s", defect, detail)
				}
				if defect == "hash-mismatch" {
					if !strings.Contains(detail, "hashkv mismatch") {
						t.Fatal(detail)
					}
				} else if !strings.Contains(detail, "missing 1 acknowledged writes") {
					t.Fatal(detail)
				}
			}
		})
	}
}

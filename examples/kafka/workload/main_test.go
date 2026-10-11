package main

import (
	"github.com/twmb/franz-go/pkg/kgo"
	"testing"
)

func TestReadbackRejectsChangedOrUnexpectedRecords(t *testing.T) {
	for _, record := range []*kgo.Record{
		{Partition: 0, Offset: 7, Value: []byte("changed")},
		{Partition: 0, Offset: 8, Value: []byte("expected")},
	} {
		remaining := map[position]string{{0, 7}: "expected"}
		if verify(record, remaining) == nil || len(remaining) != 1 {
			t.Fatal("bad readback accepted or acknowledgment removed")
		}
	}
	remaining := map[position]string{{0, 7}: "expected"}
	if err := verify(&kgo.Record{Partition: 0, Offset: 7, Value: []byte("expected")}, remaining); err != nil || len(remaining) != 0 {
		t.Fatal("valid readback rejected", err)
	}
}

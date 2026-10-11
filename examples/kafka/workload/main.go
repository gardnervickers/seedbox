// Write ten records to a replicated topic and read every acknowledgment back.
package main

import (
	"context"
	"errors"
	"flag"
	"fmt"
	"os"
	"strings"
	"time"

	"github.com/twmb/franz-go/pkg/kadm"
	"github.com/twmb/franz-go/pkg/kerr"
	"github.com/twmb/franz-go/pkg/kgo"
)

type position struct {
	partition int32
	offset    int64
}

func verify(record *kgo.Record, remaining map[position]string) error {
	at := position{record.Partition, record.Offset}
	want, ok := remaining[at]
	if !ok || string(record.Value) != want {
		return fmt.Errorf("unexpected record at %d/%d: %q", at.partition, at.offset, record.Value)
	}
	delete(remaining, at)
	return nil
}

func run(brokers string) error {
	const topic = "seedbox-example"
	client, err := kgo.NewClient(
		kgo.SeedBrokers(strings.Split(brokers, ",")...),
		kgo.DefaultProduceTopic(topic),
		kgo.RequiredAcks(kgo.AllISRAcks()),
		kgo.ConsumePartitions(map[string]map[int32]kgo.Offset{
			topic: {0: kgo.NewOffset().AtStart()},
		}),
	)
	if err != nil {
		return err
	}
	defer client.Close()
	admin := kadm.NewClient(client)
	// Startup includes controller election and registration of all three brokers.
	ctx, cancel := context.WithTimeout(context.Background(), 5*time.Minute)
	defer cancel()
	minISR := "2"
	for {
		result, err := admin.CreateTopic(ctx, 1, 3, map[string]*string{"min.insync.replicas": &minISR}, topic)
		if err == nil {
			err = result.Err
		}
		if err == nil || errors.Is(err, kerr.TopicAlreadyExists) {
			break
		}
		if ctx.Err() != nil {
			return fmt.Errorf("create topic: %w", err)
		}
		time.Sleep(time.Second)
	}
	fmt.Println("READY three-replica topic")
	records := make([]*kgo.Record, 10)
	for i := range records {
		records[i] = &kgo.Record{Value: []byte(fmt.Sprintf("record-%02d", i))}
	}
	remaining := make(map[position]string, len(records))
	for _, result := range client.ProduceSync(ctx, records...) {
		if result.Err != nil {
			return fmt.Errorf("produce: %w", result.Err)
		}
		remaining[position{result.Record.Partition, result.Record.Offset}] = string(result.Record.Value)
	}
	if len(remaining) != len(records) {
		return fmt.Errorf("acknowledgments reused a record position")
	}
	for len(remaining) > 0 {
		fetches := client.PollFetches(ctx)
		if err := fetches.Err(); err != nil {
			return fmt.Errorf("consume: %w", err)
		}
		for _, record := range fetches.Records() {
			if err := verify(record, remaining); err != nil {
				return err
			}
		}
	}
	fmt.Println("RESULT pass: 10 acknowledged records read back unchanged")
	return nil
}

func main() {
	brokers := flag.String("brokers", "localhost:9092", "comma-separated bootstrap brokers")
	flag.Parse()
	if err := run(*brokers); err != nil {
		fmt.Println("RESULT fail:", err)
		os.Exit(1)
	}
}

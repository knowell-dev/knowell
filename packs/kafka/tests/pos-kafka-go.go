package events

import (
	"context"

	"github.com/segmentio/kafka-go"
)

func NewWriter(brokers []string) *kafka.Writer {
	return &kafka.Writer{Addr: kafka.TCP(brokers...), Topic: "rack.moved"}
}

func NewRackReader(brokers []string) *kafka.Reader {
	return kafka.NewReader(kafka.ReaderConfig{Brokers: brokers, GroupID: "racks", Topic: "rack.created"})
}

func newReader(brokers []string, topic string) *kafka.Reader {
	return kafka.NewReader(kafka.ReaderConfig{Brokers: brokers, Topic: topic})
}

func Start(ctx context.Context, brokers []string) {
	r := newReader(brokers, "rack.removed")
	defer r.Close()
	_ = ctx
}

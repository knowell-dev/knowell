package bus

import "github.com/nats-io/nats.go"

func Wire(nc *nats.Conn) error {
	if err := nc.Publish("conveyor.started", []byte("1")); err != nil {
		return err
	}
	_, err := nc.QueueSubscribe("conveyor.stopped", "workers", func(m *nats.Msg) {})
	return err
}

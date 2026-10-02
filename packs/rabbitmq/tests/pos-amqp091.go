package mq

import amqp "github.com/rabbitmq/amqp091-go"

func Publish(ch *amqp.Channel, body []byte) error {
	return ch.Publish("", "forklift.serviced", false, false, amqp.Publishing{Body: body})
}

func Consume(ch *amqp.Channel) (<-chan amqp.Delivery, error) {
	return ch.Consume("forklift.broken", "", true, false, false, false, nil)
}

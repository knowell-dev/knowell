package com.example.yard;

import org.springframework.kafka.annotation.KafkaListener;
import org.springframework.kafka.core.KafkaTemplate;

public class YardEvents {
    private final KafkaTemplate<String, String> kafkaTemplate;

    public YardEvents(KafkaTemplate<String, String> kafkaTemplate) {
        this.kafkaTemplate = kafkaTemplate;
    }

    public void gateOpened(String id) {
        kafkaTemplate.send("yard.gate.opened", id);
    }

    @KafkaListener(topics = "yard.truck.arrived", groupId = "yard")
    public void onArrival(String payload) {
    }

    @KafkaListener(topics = {"yard.truck.left", "yard.truck.lost"})
    public void onDeparture(String payload) {
    }
}

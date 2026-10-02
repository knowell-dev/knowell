package com.example.crates;

import org.springframework.amqp.rabbit.annotation.RabbitListener;
import org.springframework.amqp.rabbit.core.RabbitTemplate;

public class CrateEvents {
    private final RabbitTemplate rabbitTemplate;

    public CrateEvents(RabbitTemplate rabbitTemplate) {
        this.rabbitTemplate = rabbitTemplate;
    }

    public void sealed(String payload) {
        rabbitTemplate.convertAndSend("warehouse", "crate.sealed", payload);
    }

    @RabbitListener(queues = "crate.opened")
    public void onOpened(String payload) {
    }
}

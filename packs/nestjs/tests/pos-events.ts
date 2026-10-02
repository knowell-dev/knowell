import { Controller, Get } from "@nestjs/common";
import { EventPattern, MessagePattern, Payload } from "@nestjs/microservices";

const HEALTH_TOPIC = "gadget.health";

@Controller()
class GadgetsController {
  @Get("health")
  health() {
    return "ok";
  }

  @EventPattern("gadget.created")
  onCreated(@Payload() payload: unknown) {
    return payload;
  }

  @MessagePattern(HEALTH_TOPIC)
  onHealth() {
    return true;
  }
}

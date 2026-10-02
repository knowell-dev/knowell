package com.example.depot;

import org.springframework.cloud.openfeign.FeignClient;
import org.springframework.web.bind.annotation.GetMapping;
import org.springframework.web.bind.annotation.PathVariable;

// A Feign client declares calls to another service; it serves nothing.
@FeignClient(name = "depots")
public interface DepotClient {
    @GetMapping("/v1/depots/{depotId}")
    Depot get(@PathVariable("depotId") String depotId);
}

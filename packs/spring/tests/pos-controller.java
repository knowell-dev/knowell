package com.example.depot;

import org.springframework.web.bind.annotation.*;

@RestController
@RequestMapping("/v1/depots")
public class DepotController {

    @GetMapping("/{depotId}")
    public Depot get(@PathVariable String depotId) {
        return null;
    }

    @PostMapping
    public Depot create(@RequestBody Depot depot) {
        return depot;
    }

    @RequestMapping(value = "/{depotId}/close", method = RequestMethod.PUT)
    public void close(@PathVariable String depotId) {
    }
}

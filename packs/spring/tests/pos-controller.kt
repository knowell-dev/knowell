package com.example.depot

import org.springframework.web.bind.annotation.DeleteMapping
import org.springframework.web.bind.annotation.GetMapping
import org.springframework.web.bind.annotation.PathVariable
import org.springframework.web.bind.annotation.RequestMapping
import org.springframework.web.bind.annotation.RestController

@RestController
@RequestMapping("/v1/docks")
class DockController {
    @GetMapping("/{dockId}")
    fun get(@PathVariable dockId: String): String = dockId

    @DeleteMapping("/{dockId}")
    fun delete(@PathVariable dockId: String) {
    }
}

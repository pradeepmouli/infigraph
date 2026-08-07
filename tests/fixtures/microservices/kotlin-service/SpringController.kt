package com.example.labelprinting.input.rest

import org.springframework.web.bind.annotation.GetMapping
import org.springframework.web.bind.annotation.PathVariable
import org.springframework.web.bind.annotation.PostMapping
import org.springframework.web.bind.annotation.RequestMapping
import org.springframework.web.bind.annotation.RestController

@RestController
@RequestMapping("/api/v1/label-templates")
class LabelRenderController(
    private val renderService: String
) {
    @PostMapping("/{templateId}/render")
    fun render(@PathVariable templateId: String): String {
        return "rendered"
    }

    @GetMapping
    fun list(): String {
        return "list"
    }

    @GetMapping("/{templateId}")
    fun getOne(@PathVariable templateId: String): String {
        return "one"
    }
}

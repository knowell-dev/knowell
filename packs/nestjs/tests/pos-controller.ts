import { Body, Controller, Delete, Get, Param, Post } from "@nestjs/common";

@Controller("v1/widgets")
export class WidgetsController {
  @Get()
  list() {
    return [];
  }

  @Get(":widgetId")
  get(@Param("widgetId") id: string) {
    return { id };
  }

  @Post(":widgetId/archive")
  archive(@Param("widgetId") id: string, @Body() body: unknown) {
    return { id, body };
  }

  @Delete("/:widgetId/")
  remove(@Param("widgetId") id: string) {
    return id;
  }
}

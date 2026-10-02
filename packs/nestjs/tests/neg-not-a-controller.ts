import { Injectable } from "@nestjs/common";

function Get(path: string) {
  return (target: unknown, key: string) => [target, key, path];
}

// A route-like decorator on a class without @Controller is not a Nest route.
export class NotAController {
  @Get("not-a-route")
  handler() {
    return 1;
  }
}

@Injectable()
export class WidgetCache {
  private readonly items = new Map<string, string>();

  get(id: string) {
    return this.items.get(id);
  }
}

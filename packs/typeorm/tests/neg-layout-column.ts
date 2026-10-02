import { Column } from "./ui/layout";

// A decorator named Column outside an @Entity class.
export class Grid {
  @Column()
  width!: number;
}

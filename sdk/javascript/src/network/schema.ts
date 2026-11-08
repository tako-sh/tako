import ts from "typescript";

export type Schema = boolean | { [key: string]: unknown };
export interface NetworkSchema {
  $schema: string;
  $comment: string;
  $defs: Record<string, Schema>;
  "x-tako": { version: 1; app: string; primitives: Array<Record<string, unknown>> };
}

/** Export the JSON wire shape, independently of TypeScript declaration syntax. */
export class SchemaGraph {
  readonly definitions: Record<string, Schema> = {};
  private readonly names = new Map<ts.Type, string>();
  constructor(
    private readonly checker: ts.TypeChecker,
    private readonly location: ts.Node,
  ) {}

  print(type: ts.Type, result = false): Schema {
    const f = type.flags;
    if (f & (ts.TypeFlags.Any | ts.TypeFlags.Unknown)) return true;
    if (f & ts.TypeFlags.Never) return false;
    if (f & (ts.TypeFlags.Void | ts.TypeFlags.Undefined)) {
      if (result) return { type: "null" };
      throw new Error(
        "Network schemas cannot represent undefined JSON values; use an optional property or null",
      );
    }
    if (f & ts.TypeFlags.Null) return { type: "null" };
    if (type.isStringLiteral() || type.isNumberLiteral()) return { const: type.value };
    if (f & ts.TypeFlags.BooleanLiteral)
      return { const: this.checker.typeToString(type) === "true" };
    if (f & ts.TypeFlags.String) return { type: "string" };
    if (f & ts.TypeFlags.Number) return { type: "number" };
    if (f & ts.TypeFlags.Boolean) return { type: "boolean" };
    if (f & ts.TypeFlags.TemplateLiteral) {
      const template = type as ts.TemplateLiteralType;
      const escape = (s: string) => s.replace(/[.*+?^${}()|[\]\\]/g, "\\$&");
      const types = template.types.map((t) => this.print(t));
      return {
        type: "string",
        pattern:
          "^" +
          template.texts
            .map((text, i) => escape(text) + (i < types.length ? "[\\s\\S]*" : ""))
            .join("") +
          "$",
        "x-tako-template": { texts: template.texts, types },
      };
    }
    if (type.isUnion()) return { anyOf: type.types.map((t) => this.print(t, result)) };
    const previous = this.names.get(type);
    if (previous) return { $ref: `#/$defs/${previous}` };
    if (this.names.size >= 4096) throw new Error("Network schema defines too many types");
    const name = `Type${this.names.size + 1}`;
    this.names.set(type, name);
    this.definitions[name] = this.compound(type);
    return { $ref: `#/$defs/${name}` };
  }

  private present(type: ts.Type): Schema {
    if (type.isUnion()) {
      const values = type.types.filter((t) => !(t.flags & ts.TypeFlags.Undefined));
      return values.length === 1
        ? this.print(values[0]!)
        : { anyOf: values.map((t) => this.print(t)) };
    }
    if (type.flags & ts.TypeFlags.Undefined) return false;
    return this.print(type);
  }

  private compound(type: ts.Type): Schema {
    const objectIntersection =
      type.isIntersection() && type.types.every((part) => !!(part.flags & ts.TypeFlags.Object));
    if (this.checker.isTupleType(type)) {
      const ref = type as ts.TypeReference;
      const tuple = ref.target as ts.TupleType;
      const args = this.checker.getTypeArguments(ref);
      const prefixItems: Schema[] = [];
      let items: Schema = false;
      let minItems = 0;
      for (let i = 0; i < args.length; i++) {
        const flag = tuple.elementFlags[i] ?? ts.ElementFlags.Required;
        if (flag & ts.ElementFlags.Rest) {
          if (i !== args.length - 1)
            throw new Error("Network schemas require tuple rest elements to be last");
          items = this.print(args[i]!);
        } else {
          if (flag & ts.ElementFlags.Required) minItems++;
          prefixItems.push(this.present(args[i]!));
        }
      }
      return {
        type: "array",
        ...(prefixItems.length ? { prefixItems } : {}),
        minItems,
        items,
        ...(items === false ? { maxItems: prefixItems.length } : {}),
        ...(tuple.readonly ? { readOnly: true } : {}),
      };
    }
    if (this.checker.isArrayType(type)) {
      return {
        type: "array",
        items: this.print(this.checker.getTypeArguments(type as ts.TypeReference)[0]!),
        ...(type.getSymbol()?.getName() === "ReadonlyArray" ? { readOnly: true } : {}),
      };
    }
    if (
      (!(type.flags & ts.TypeFlags.Object) && !objectIntersection) ||
      type.getCallSignatures().length ||
      type.getConstructSignatures().length
    ) {
      throw new Error(
        `Network schemas require JSON data; unsupported type: ${this.checker.typeToString(type)}`,
      );
    }
    const properties: Record<string, Schema> = {};
    const required: string[] = [];
    for (const property of this.checker.getPropertiesOfType(type)) {
      if (property.getName().startsWith("__@"))
        throw new Error("Symbol properties cannot be exported in network schemas");
      const value = this.checker.getTypeOfSymbolAtLocation(
        property,
        property.valueDeclaration ?? this.location,
      );
      const optional = !!(property.flags & ts.SymbolFlags.Optional);
      properties[property.getName()] = optional ? this.present(value) : this.print(value);
      if (!optional) required.push(property.getName());
    }
    const indexes = this.checker.getIndexInfosOfType(type);
    if (indexes.length > 1)
      throw new Error("Network schemas support one JSON object index signature");
    return {
      type: "object",
      properties,
      required,
      additionalProperties: indexes[0] ? this.print(indexes[0].type) : false,
    };
  }
}

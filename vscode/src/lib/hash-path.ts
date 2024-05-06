import { createHash } from "crypto";

export function hashPath(path: string): string {
	return createHash("sha1").update(path).digest("hex");
}

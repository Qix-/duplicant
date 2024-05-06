import path from "node:path";
import { Duplicant } from "./duplicant-reader";

export async function findDuplicantFile(rootPath: string): Promise<Duplicant> {
	const paths = [
		path.join(rootPath, "dupl.bin"),
		path.join(rootPath, ".git", "dupl.bin"),
		rootPath + ".dupl",
		rootPath + ".dupl.bin",
	];

	return Promise.allSettled(paths.map(Duplicant.open)).then((results) => {
		const errors = [];
		for (const result of results) {
			if (result.status === "fulfilled") {
				return result.value;
			} else {
				errors.push(result.reason);
			}
		}

		throw new Error(
			"No duplicant file found: " +
				rootPath +
				"\n" +
				errors.map((e) => `  ${e.message}`).join(`\n`),
		);
	});
}

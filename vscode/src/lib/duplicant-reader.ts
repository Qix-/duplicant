import fs from "node:fs/promises";
import { hashPath } from "./hash-path";

const maxSafeIntegerBigInt = BigInt(Number.MAX_SAFE_INTEGER);
const minSafeIntegerBigInt = BigInt(Number.MIN_SAFE_INTEGER);

function convertBigIntToNumber(n: bigint): number {
	if (n > maxSafeIntegerBigInt || n < minSafeIntegerBigInt) {
		throw new Error(
			"The BigInt is too large to convert to a number without loss of precision.",
		);
	}

	return Number(n);
}

class ByteReader {
	#file: fs.FileHandle;
	#offset: number;

	constructor(file: fs.FileHandle) {
		this.#file = file;
		this.#offset = 0;
	}

	async readByte(): Promise<number> {
		const buffer = Buffer.alloc(1);
		await this.#file.read(buffer, 0, 1, this.#offset);
		this.#offset += 1;
		return buffer.readUInt8(0);
	}

	async readBytes(n: number): Promise<Buffer> {
		const buffer = Buffer.alloc(n);
		await this.#file.read(buffer, 0, n, this.#offset);
		this.#offset += n;
		return buffer;
	}

	async readU32(): Promise<number> {
		const buffer = await this.readBytes(4);
		return buffer.readUInt32LE(0);
	}

	async readU64(): Promise<number> {
		const buffer = await this.readBytes(8);
		return convertBigIntToNumber(buffer.readBigUInt64LE());
	}

	async readSha(): Promise<string> {
		const buffer = await this.readBytes(20);
		return buffer.toString("hex");
	}

	// Reads a NUL-terminated UTF-8 string
	async readString(): Promise<string> {
		const chars: number[] = [];

		while (true) {
			const char = await this.readByte();
			if (char === 0) {
				break;
			}

			chars.push(char);
		}

		return Buffer.from(chars).toString("utf8");
	}

	seek(offset: number) {
		this.#offset = offset;
	}
}

type DuplHeader = {
	magic: number;
	max_similarity_count: number;
	num_paths: number;
	paths_offset: number;
};

type DuplPathMeta = {
	// Hex representation
	sha1: string;
	line_count: number;
	lines_offset: number;
	path_name_offset: number;
};

type DuplLineMeta = {
	line_no: number;
	similarity_count: number;
	similarities_offset: number;
};

type DuplSimilarityMeta = {
	path_offset: number;
	line_no: number;
};

export class DuplicantSimilarity {
	#reader: ByteReader;
	#meta: DuplSimilarityMeta;
	#duplication: Duplicant;

	constructor(
		duplicant: Duplicant,
		reader: ByteReader,
		meta: DuplSimilarityMeta,
	) {
		this.#reader = reader;
		this.#meta = meta;
		this.#duplication = duplicant;
	}

	get lineNo(): number {
		return this.#meta.line_no;
	}

	async getPath(): Promise<DuplicantPath> {
		return this.#duplication.getPathAtOffset(this.#meta.path_offset);
	}
}

export class DuplicantLine {
	#reader: ByteReader;
	#meta: DuplLineMeta;
	#duplicant: Duplicant;

	constructor(duplicant: Duplicant, reader: ByteReader, meta: DuplLineMeta) {
		this.#reader = reader;
		this.#meta = meta;
		this.#duplicant = duplicant;
	}

	get lineNo(): number {
		return this.#meta.line_no;
	}

	get similarityCount(): number {
		return this.#meta.similarity_count;
	}

	async getSimilarityMetas(): Promise<DuplSimilarityMeta[]> {
		this.#reader.seek(this.#meta.similarities_offset);

		const similarities: DuplSimilarityMeta[] = [];
		for (let i = 0; i < this.#meta.similarity_count; i++) {
			const path_offset = await this.#reader.readU64();
			const line_no = await this.#reader.readU32();
			similarities.push({ path_offset, line_no });
		}

		return similarities;
	}

	async getSimilarities(): Promise<DuplicantSimilarity[]> {
		const metas = await this.getSimilarityMetas();
		return metas.map(
			(meta) =>
				new DuplicantSimilarity(this.#duplicant, this.#reader, meta),
		);
	}
}

export class DuplicantLines {
	#lines: DuplicantLine[];

	constructor(lines: DuplicantLine[]) {
		this.#lines = lines;

		// Make sure it's sorted.
		this.#lines.sort((a, b) => a.lineNo - b.lineNo);
	}

	get length(): number {
		return this.#lines.length;
	}

	[Symbol.iterator]() {
		return this.#lines[Symbol.iterator]();
	}

	find(line_no: number): DuplicantLine | undefined {
		// Perform a binary search
		let low = 0;
		let high = this.#lines.length - 1;

		while (low <= high) {
			const mid = (low + high) >> 1;
			const midLine = this.#lines[mid];

			if (midLine.lineNo < line_no) {
				low = mid + 1;
			} else if (midLine.lineNo > line_no) {
				high = mid - 1;
			} else {
				return midLine;
			}
		}
	}
}

export class DuplicantPath {
	#reader: ByteReader;
	#path: string;
	#pathMeta: DuplPathMeta;
	#duplicant: Duplicant;

	constructor(
		duplicant: Duplicant,
		reader: ByteReader,
		path: string,
		pathMeta: DuplPathMeta,
	) {
		this.#reader = reader;
		this.#path = path;
		this.#pathMeta = pathMeta;
		this.#duplicant = duplicant;
	}

	get path(): string {
		return this.#path;
	}

	get lineCount(): number {
		return this.#pathMeta.line_count;
	}

	async getLineMetas(): Promise<DuplLineMeta[]> {
		// Not every line is mapped; we have to iterate through all lines to find the one we're looking for
		this.#reader.seek(this.#pathMeta.lines_offset);

		const metas: DuplLineMeta[] = [];
		for (let i = 0; i < this.#pathMeta.line_count; i++) {
			const line_no = await this.#reader.readU32();
			const similarity_count = await this.#reader.readU32();
			const similarities_offset = await this.#reader.readU64();

			metas.push({ line_no, similarity_count, similarities_offset });
		}

		return metas;
	}

	async getLines(): Promise<DuplicantLines> {
		return new DuplicantLines(
			(await this.getLineMetas()).map(
				(meta) =>
					new DuplicantLine(this.#duplicant, this.#reader, meta),
			),
		);
	}
}

export class Duplicant {
	#reader: ByteReader;
	#path: string;
	#meta: DuplHeader;

	private constructor(reader: ByteReader, path: string, meta: DuplHeader) {
		this.#reader = reader;
		this.#path = path;
		this.#meta = meta;
	}

	get path(): string {
		return this.#path;
	}

	get maxSimilarityCount(): number {
		return this.#meta.max_similarity_count;
	}

	async #readPathMetaAtCurrentPosition(): Promise<DuplPathMeta> {
		const sha1 = await this.#reader.readSha();
		const line_count = await this.#reader.readU32();
		const lines_offset = await this.#reader.readU64();
		const path_name_offset = await this.#reader.readU64();

		return { sha1, line_count, lines_offset, path_name_offset };
	}

	async findPathMeta(path: string): Promise<DuplPathMeta | undefined> {
		this.#reader.seek(this.#meta.paths_offset);

		const pathSha = hashPath(path);

		for (let i = 0; i < this.#meta.num_paths; i++) {
			const meta = await this.#readPathMetaAtCurrentPosition();
			if (meta.sha1 === pathSha) {
				return meta;
			}
		}
	}

	async findPath(path: string): Promise<DuplicantPath | undefined> {
		const pathMeta = await this.findPathMeta(path);
		if (!pathMeta) {
			return;
		}

		return new DuplicantPath(this, this.#reader, path, pathMeta);
	}

	async getPathMetaAtOffset(offset: number): Promise<DuplPathMeta> {
		this.#reader.seek(offset);
		return this.#readPathMetaAtCurrentPosition();
	}

	async getPathAtOffset(offset: number): Promise<DuplicantPath> {
		const pathMeta = await this.getPathMetaAtOffset(offset);
		this.#reader.seek(pathMeta.path_name_offset);
		const pathName = await this.#reader.readString();
		return new DuplicantPath(this, this.#reader, pathName, pathMeta);
	}

	static async open(path: string): Promise<Duplicant> {
		// Open the file for reading
		const file = await fs.open(path, "r");
		const reader = new ByteReader(file);
		// Read the header

		reader.seek(0);
		const magic = await reader.readU32();
		const max_similarity_count = await reader.readU32();
		const num_paths = await reader.readU64();
		const paths_offset = await reader.readU64();
		const header = { magic, max_similarity_count, num_paths, paths_offset };

		// Validate the magic number
		if (header.magic !== 0x4c505544) {
			throw new Error("Invalid magic number");
		}

		return new Duplicant(reader, path, header);
	}
}

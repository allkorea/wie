const MAX_SAVE_BYTES = 16 * 1024 * 1024;

const writes = new Map<Promise<unknown>, number>();
const connections = new Set<IDBDatabase>();
const stores = new Map<string, Promise<IndexedDBStore>>();
let writeError: unknown;
let generation = 0;
let storageReady: (() => void) | undefined;
export function setStorageReadyListener(listener: (() => void) | undefined) {
  storageReady = listener;
}
function wakeWhenSettled<T>(promise: Promise<T>): Promise<T> {
  const ready = storageReady;
  const owner = generation;
  return ready
    ? promise.finally(() => {
        if (owner === generation) ready();
      })
    : promise;
}
export class GameStorageError extends Error {
  override name = "GameStorageError";
}

export function openDatabase(name: string, store: string): Promise<IDBDatabase> {
  return new Promise((resolve, reject) => {
    let blocked = false;
    const request = indexedDB.open(name);
    request.onupgradeneeded = () => request.result.createObjectStore(store);
    request.onerror = () => reject(request.error);
    request.onblocked = () => {
      blocked = true;
      reject(new GameStorageError("다른 창에서 저장 공간을 사용하고 있습니다. 다른 게임 창을 닫아 주세요."));
    };
    request.onsuccess = () => {
      const db = request.result;
      if (blocked) {
        db.close();
        return;
      }
      db.onversionchange = () => db.close();
      resolve(db);
    };
  });
}

export function committed<T>(transaction: IDBTransaction, result: () => T): Promise<T> {
  return new Promise((resolve, reject) => {
    transaction.oncomplete = () => {
      try {
        resolve(result());
      } catch (error) {
        reject(error);
      }
    };
    transaction.onabort = () => reject(transaction.error ?? new Error("저장 작업이 취소되었습니다."));
  });
}

export async function flushWrites() {
  const owner = generation;
  const opening = await Promise.allSettled([...stores.values()]);
  const settled = await Promise.allSettled([...writes].filter(([, epoch]) => epoch === owner).map(([write]) => write));
  if (owner !== generation) throw new GameStorageError("게임의 저장 연결이 종료되었습니다.");
  const failure = [...opening, ...settled].find((result) => result.status === "rejected");
  if (failure?.status === "rejected") writeError ??= failure.reason;
  if (writeError)
    throw new GameStorageError("저장 공간에 기록하지 못했습니다. 저장 공간을 확인한 뒤 다시 실행해 주세요.", {
      cause: writeError,
    });
}

export function closeCoreStores() {
  generation++;
  stores.clear();
  for (const db of connections) db.close();
  connections.clear();
  writeError = undefined;
}

export class IndexedDBStore {
  private db: IDBDatabase;
  private store: string;
  private generation: number;
  private closed = false;
  private constructor(db: IDBDatabase, store: string, owner: number) {
    this.db = db;
    this.store = store;
    this.generation = owner;
  }
  static open(name: string, store: string): Promise<IndexedDBStore> {
    const key = JSON.stringify([name, store]);
    const cached = stores.get(key);
    if (cached) return wakeWhenSettled(cached);
    const owner = generation;
    const opening = openDatabase(name, store)
      .then((db) => {
        if (owner !== generation) {
          db.close();
          throw new GameStorageError("게임의 저장 연결이 종료되었습니다.");
        }
        connections.add(db);
        const handle = new IndexedDBStore(db, store, owner);
        const invalidate = () => {
          handle.closed = true;
          connections.delete(db);
          if (stores.get(key) === opening) stores.delete(key);
        };
        db.addEventListener("versionchange", invalidate);
        db.addEventListener("close", invalidate);
        return handle;
      })
      .catch((error: unknown) => {
        if (stores.get(key) === opening) stores.delete(key);
        throw error;
      });
    stores.set(key, opening);
    return wakeWhenSettled(opening);
  }
  private run<T>(mode: IDBTransactionMode, request: (store: IDBObjectStore) => IDBRequest<T>) {
    if (this.closed || this.generation !== generation)
      return Promise.reject(new GameStorageError("게임의 저장 연결이 종료되었습니다."));
    let done: Promise<T>;
    try {
      const transaction = this.db.transaction(this.store, mode);
      const operation = request(transaction.objectStore(this.store));
      done = committed(transaction, () => operation.result);
    } catch (error) {
      if (mode === "readwrite") writeError = error;
      return Promise.reject(error);
    }
    if (mode === "readwrite") {
      writes.set(done, this.generation);
      void done.then(
        () => writes.delete(done),
        (error: unknown) => {
          writes.delete(done);
          if (this.generation === generation) writeError = error;
        },
      );
    }
    return wakeWhenSettled(done);
  }
  get_all_keys() {
    return this.run("readonly", (store) => store.getAllKeys());
  }
  get(key: IDBValidKey): Promise<Uint8Array | undefined> {
    return this.run("readonly", (store) => store.get(key));
  }
  async contains(key: IDBValidKey): Promise<boolean> {
    return (await this.run("readonly", (store) => store.count(key))) !== 0;
  }
  async size(key: IDBValidKey): Promise<number | undefined> {
    return this.bytes(await this.get(key))?.byteLength;
  }
  async read(key: IDBValidKey, offset: number, count: number): Promise<Uint8Array | undefined> {
    if (!Number.isSafeInteger(offset) || offset < 0 || !Number.isSafeInteger(count) || count < 0)
      throw new GameStorageError("저장 파일의 읽기 범위가 올바르지 않습니다.");
    const bytes = this.bytes(await this.get(key));
    return bytes?.slice(offset, offset + Math.min(count, bytes.byteLength));
  }
  private bytes(value: unknown): Uint8Array | undefined {
    if (value === undefined || value instanceof Uint8Array) return value;
    throw new GameStorageError("저장 데이터의 형식이 올바르지 않습니다.");
  }
  private length(value: number): void {
    if (!Number.isSafeInteger(value) || value < 0 || value > MAX_SAVE_BYTES)
      throw new GameStorageError("저장 파일의 크기가 허용 범위를 벗어났습니다.");
  }
  private async modify(key: IDBValidKey, initial: Uint8Array, update: (bytes: Uint8Array) => Uint8Array): Promise<void> {
    let failure: unknown;
    await this.run("readwrite", (store) => {
      const request = store.get(key);
      request.onsuccess = () => {
        try {
          store.put(update(this.bytes(request.result) ?? initial), key);
        } catch (error) {
          failure = error;
          store.transaction.abort();
        }
      };
      return request;
    }).catch((error: unknown) => {
      throw failure ?? error;
    });
  }
  async write(key: IDBValidKey, offset: number, data: Uint8Array, initial = new Uint8Array()): Promise<number> {
    this.length(offset);
    this.length(offset + data.byteLength);
    // Own the input until its request callback executes; the WASM caller may reuse it.
    const input = data.slice();
    await this.modify(key, initial.slice(), (bytes) => {
      const length = Math.max(bytes.byteLength, offset + input.byteLength);
      this.length(length);
      if (length !== bytes.byteLength) {
        const expanded = new Uint8Array(length);
        expanded.set(bytes);
        bytes = expanded;
      }
      bytes.set(input, offset);
      return bytes;
    });
    return input.byteLength;
  }
  async truncate(key: IDBValidKey, length: number, initial = new Uint8Array()): Promise<void> {
    this.length(length);
    await this.modify(key, initial.slice(), (bytes) => {
      if (length <= bytes.byteLength) return bytes.slice(0, length);
      const expanded = new Uint8Array(length);
      expanded.set(bytes);
      return expanded;
    });
  }
  private recordId(key: IDBValidKey, prefix: string): number | undefined {
    if (typeof key !== "string" || !key.startsWith(prefix)) return;
    const suffix = key.slice(prefix.length);
    if (!/^[+]?[0-9]+$/u.test(suffix)) return;
    const id = Number(suffix);
    return Number.isSafeInteger(id) && id <= 0xffffffff ? id : undefined;
  }
  async add_record(prefix: string, data: Uint8Array): Promise<number> {
    const input = data.slice();
    let id = 1;
    let failure: unknown;
    await this.run("readwrite", (store) => {
      const request = store.getAllKeys();
      request.onsuccess = () => {
        try {
          for (const key of request.result) {
            const existing = this.recordId(key, prefix);
            if (existing !== undefined) id = Math.max(id, existing + 1);
          }
          if (id > 0xffffffff) throw new GameStorageError("레코드 ID가 허용 범위를 벗어났습니다.");
          store.put(input, prefix + id);
        } catch (error) {
          failure = error;
          store.transaction.abort();
        }
      };
      return request;
    }).catch((error: unknown) => { throw failure ?? error; });
    return id;
  }
  async usage(): Promise<number> {
    let total = 0;
    let failure: unknown;
    await this.run("readonly", (store) => {
      const request = store.openCursor();
      request.onsuccess = () => {
        const cursor = request.result;
        if (!cursor) return;
        try {
          total += this.bytes(cursor.value)?.byteLength ?? 0;
          if (!Number.isSafeInteger(total)) throw new GameStorageError("저장 크기가 허용 범위를 벗어났습니다.");
          cursor.continue();
        } catch (error) {
          failure = error;
          store.transaction.abort();
        }
      };
      return request;
    }).catch((error: unknown) => { throw failure ?? error; });
    return total;
  }
  async delete_records(prefix: string): Promise<boolean> {
    let deleted = false;
    await this.run("readwrite", (store) => {
      const request = store.openKeyCursor();
      request.onsuccess = () => {
        const cursor = request.result;
        if (!cursor) return;
        if (this.recordId(cursor.key, prefix) !== undefined) {
          store.delete(cursor.primaryKey);
          deleted = true;
        }
        cursor.continue();
      };
      return request;
    });
    return deleted;
  }
  async set(key: IDBValidKey, data: Uint8Array) {
    await this.run("readwrite", (store) => store.put(data, key));
  }
  async delete(key: IDBValidKey) {
    await this.run("readwrite", (store) => store.delete(key));
  }
}

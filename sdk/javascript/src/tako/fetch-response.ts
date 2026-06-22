type ResponseShim = Response & {
  readonly _response?: unknown;
};

/**
 * Normalize Response objects from runtimes that wrap the native web response.
 *
 * @internal Used by generated framework entrypoints.
 */
export function normalizeFetchResponse(response: Response): Response {
  const nativeResponse = (response as ResponseShim)._response;
  if (nativeResponse instanceof Response) {
    return nativeResponse;
  }

  return response;
}

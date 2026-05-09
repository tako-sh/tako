type ResponseShim = Response & {
  readonly _response?: unknown;
};

export function normalizeFetchResponse(response: Response): Response {
  const nativeResponse = (response as ResponseShim)._response;
  if (nativeResponse instanceof Response) {
    return nativeResponse;
  }

  return response;
}

import { lazy, Suspense, useEffect, useState } from "react";
import { ArrowRight } from "lucide-react";
import { CaseTable, Pagination, pageSizes } from "./CaseTable";
import { ReportFilters } from "./ReportFilters";
import { useRead } from "./api";
import {
  dateTime,
  href,
  pinReportWindow,
  readQuery,
  reportWindow,
} from "./cases";
import "./reporting.css";

export { CaseTable };
const Analytics = lazy(() => import("./Analytics"));
const numbers = new Intl.NumberFormat();

export function Reports({
  page,
  params,
  navigate,
  revision,
  staffToken,
  reportHref = href,
}) {
  const compact = page === "overview";
  const update = (change) =>
    navigate(page, { ...params, page: "", case: "", owner: "", ...change });
  const pageSize = pageSizes.includes(Number(params.page_size))
    ? Number(params.page_size)
    : 20;
  const requested = Number(params.page);
  const pageNumber =
    !compact && Number.isSafeInteger(requested) && requested > 0
      ? Math.min(requested, Math.floor(10000 / pageSize) + 1)
      : 1;
  const [now, setNow] = useState(Date.now);
  useEffect(() => setNow(Date.now()), [revision, params.period, params.at]);
  const window = reportWindow(params, now);
  const filters = {
    ...params,
    ...window,
    page: pageNumber,
    page_size: compact ? 5 : pageSize,
    sort: params.sort || "created_at_desc",
  };
  const base = staffToken
    ? "/staff-api/v2/cases"
    : "/product-demo/observer/cases";
  const listPath = window ? base + readQuery(filters, true) : null;
  const summaryPath =
    compact && window ? base + "/overview" + readQuery(filters, false) : null;
  const rolling =
    window && !params.at && pageNumber === 1 && params.period !== "custom";
  // Only the clock may change under this identity. Pinned/custom ranges and
  // every change of scope, screen, filters, sort, or pagination read afresh.
  const identity = rolling
    ? `${base}/${page}/${params.period || "7"}${readQuery({ ...filters, from: "", to: "" }, true)}`
    : null;
  const list = useRead(
    listPath,
    revision,
    rolling ? 0 : 6500,
    identity ? identity + "/list" : listPath,
    staffToken,
  );
  const summary = useRead(
    summaryPath,
    revision,
    rolling ? 0 : 6500,
    identity && summaryPath ? identity + "/overview" : summaryPath,
    staffToken,
  );
  const received =
    list.path === listPath &&
    (list.data || list.error) &&
    (!summaryPath ||
      (summary.path === summaryPath && (summary.data || summary.error)));
  useEffect(() => {
    if (!rolling || !received) return;
    const timer = setTimeout(() => setNow(Date.now()), 6500);
    return () => clearTimeout(timer);
  }, [rolling, received, listPath, summaryPath]);
  const pinned = (result) =>
    pinReportWindow(params, result?.to ? { to: result.to } : window);
  const directory = (extra = {}, result = list.data) =>
    reportHref("cases", {
      ...pinned(result),
      case: "",
      owner: "",
      page: "",
      ...extra,
    });
  const emptyOverview =
    compact && summary.data?.total === 0 && !list.data?.total;

  return (
    <div className="report-view">
      <ReportFilters params={params} update={update} />
      {window && params.at && params.period !== "custom" && (
        <div className="report-cutoff">
          <span>
            Created through{" "}
            <time dateTime={window.to}>{dateTime(window.to)}</time>
          </span>
          <button className="text-button" onClick={() => update({ at: "" })}>
            Return to live results
          </button>
        </div>
      )}
      {!window ? (
        <p className="error-banner" role="status">
          Choose both dates in a range of up to 366 days.
        </p>
      ) : (
        <>
          {(list.error || summary.error) && (
            <p className="error-banner" role="alert">
              {list.error || summary.error}
            </p>
          )}
          {compact &&
            (summary.data ? (
              <Suspense
                fallback={
                  <p className="report-loading" role="status">
                    Loading overview…
                  </p>
                }
              >
                <Analytics
                  data={summary.data}
                  filtered={Boolean(
                    params.query ||
                      params.product ||
                      params.use_case ||
                      params.state,
                  )}
                  statusHref={(state) => directory({ state }, summary.data)}
                  select={(state) =>
                    navigate("cases", {
                      ...pinned(summary.data),
                      state,
                      page: "",
                      case: "",
                      owner: "",
                    })
                  }
                />
              </Suspense>
            ) : (
              !summary.error && (
                <p className="report-loading" role="status">
                  Loading overview…
                </p>
              )
            ))}
          {!emptyOverview && (
            <section className="panel report-directory">
              <div className="report-directory-heading">
                <div>
                  <h2>{compact ? "Recent cases" : "Case directory"}</h2>
                  {list.data && (
                    <p>
                      {numbers.format(list.data.total)} matching{" "}
                      {list.data.total === 1 ? "case" : "cases"}
                    </p>
                  )}
                </div>
                {compact ? (
                  <a className="text-button" href={directory()}>
                    View all cases <ArrowRight size={15} aria-hidden="true" />
                  </a>
                ) : (
                  <label className="report-sort">
                    Sort
                    <select
                      aria-label="Sort cases"
                      value={filters.sort}
                      onChange={(e) =>
                        navigate(page, {
                          ...pinned(list.data),
                          sort: e.target.value,
                          page: "",
                          case: "",
                          owner: "",
                        })
                      }
                    >
                      <option value="created_at_desc">Newest first</option>
                      <option value="created_at_asc">Oldest first</option>
                    </select>
                  </label>
                )}
              </div>
              {list.data ? (
                <CaseTable
                  data={list.data}
                  pageNumber={pageNumber}
                  firstPage={directory()}
                  caseHref={(c) =>
                    reportHref(page, {
                      ...pinned(list.data),
                      case: c.case_id,
                      owner: c.product,
                    })
                  }
                />
              ) : (
                !list.error && (
                  <p className="report-loading" role="status">
                    Loading cases…
                  </p>
                )
              )}
              {!compact && list.data && (
                <Pagination
                  data={list.data}
                  page={pageNumber}
                  size={pageSize}
                  pageHref={(page) => directory({ page })}
                  changeSize={(page_size) =>
                    navigate("cases", {
                      ...pinned(list.data),
                      page: "",
                      page_size,
                      case: "",
                      owner: "",
                    })
                  }
                />
              )}
            </section>
          )}
        </>
      )}
    </div>
  );
}

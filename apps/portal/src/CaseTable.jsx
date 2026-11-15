import { ChevronLeft, ChevronRight, FolderSearch } from "lucide-react";
import { Badge } from "./CaseDetail";
import { dateTime, products, useCases } from "./cases";

export const pageSizes = [5, 10, 20, 50, 100];
const numbers = new Intl.NumberFormat();
const dates = new Intl.DateTimeFormat(undefined, { dateStyle: "medium" });
const times = new Intl.DateTimeFormat(undefined, { timeStyle: "short" });

export function CaseTable({ data, caseHref, pageNumber = 1, firstPage }) {
  if (!data.cases.length)
    return (
      <div className="report-no-rows">
        <FolderSearch size={22} aria-hidden="true" />
        <strong>
          {pageNumber > 1 && data.total
            ? "No cases on this page"
            : "No matching cases"}
        </strong>
        {pageNumber > 1 && data.total ? (
          <a className="text-button" href={firstPage}>
            Return to first page
          </a>
        ) : (
          <span>Try a different time range or search.</span>
        )}
      </div>
    );
  return (
    <table className="report-grid" aria-label="Verification cases" role="table">
      <thead>
        <tr>
          {["Case ID", "Customer", "Use case", "Status", "Opened"].map(
            (label) => (
              <th key={label} scope="col">
                {label}
              </th>
            ),
          )}
        </tr>
      </thead>
      <tbody>
        {data.cases.map((c) => {
          const date = c.created_at && new Date(c.created_at);
          return (
            <tr key={c.case_id}>
              <td className="report-id-cell">
                <a
                  className="report-case-id"
                  href={caseHref(c)}
                  aria-label={"Open case " + c.case_id}
                  title={c.case_id}
                >
                  {c.case_id.split("~").at(-1).slice(0, 10).toUpperCase()}
                </a>
              </td>
              <td className="report-customer-cell">
                <span className="report-cell-label" aria-hidden="true">
                  Customer
                </span>
                <code title={c.subject_ref}>{c.subject_ref}</code>
              </td>
              <td className="report-use-cell">
                <span className="report-cell-label" aria-hidden="true">
                  Use case
                </span>
                <div>
                  {useCases[c.use_case] || c.use_case}
                  <small>{products[c.product] || c.product}</small>
                </div>
              </td>
              <td className="report-status-cell">
                <Badge state={c.state} product={c.product} />
              </td>
              <td className="report-opened-cell">
                <span className="report-cell-label" aria-hidden="true">
                  Opened
                </span>
                {date ? (
                  <time dateTime={c.created_at} title={dateTime(c.created_at)}>
                    {dates.format(date)}
                    <small>{times.format(date)}</small>
                  </time>
                ) : (
                  "—"
                )}
              </td>
            </tr>
          );
        })}
      </tbody>
    </table>
  );
}

export function Pagination({ data, page, size, changeSize, pageHref }) {
  if (data.total <= pageSizes[0] && page === 1) return null;
  const offset = (page - 1) * size;
  const limitReached =
    page * size > 10000 && offset + data.cases.length < data.total;
  const prev = page > 1;
  const next = !limitReached && page * size < data.total;
  return (
    <nav className="report-pages" aria-label="Case pages">
      <span className="report-range">
        {data.cases.length
          ? `${numbers.format(offset + 1)}–${numbers.format(offset + data.cases.length)} of ${numbers.format(data.total)}`
          : `${numbers.format(data.total)} matching cases`}
      </span>
      <label className="report-page-size">
        Per page
        <select value={size} onChange={(e) => changeSize(e.target.value)}>
          {pageSizes.map((n) => (
            <option key={n}>{n}</option>
          ))}
        </select>
      </label>
      <div className="report-page-links">
        <a
          role="link"
          href={prev ? pageHref(page - 1) : undefined}
          aria-label="Previous page"
          aria-disabled={!prev}
          className="icon-button"
        >
          <ChevronLeft size={16} />
        </a>
        <span>Page {numbers.format(page)}</span>
        <a
          role="link"
          href={next ? pageHref(page + 1) : undefined}
          aria-label="Next page"
          aria-disabled={!next}
          className="icon-button"
        >
          <ChevronRight size={16} />
        </a>
      </div>
      {limitReached && <small>Narrow the filters to see more cases.</small>}
    </nav>
  );
}

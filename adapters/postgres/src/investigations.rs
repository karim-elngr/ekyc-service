use crate::{Error, Store};
use chrono::{DateTime, Utc};
use serde::Serialize;

#[derive(Serialize, sqlx::FromRow)]
pub struct Investigation {
    pub work_id: String,
    pub kind: String,
    pub case_id: String,
    pub product: String,
    pub operation_id: String,
    pub state: String,
    pub opened_at: DateTime<Utc>,
    pub acknowledged_by: Option<String>,
    pub acknowledged_at: Option<DateTime<Utc>>,
}

const OPEN: &str = "SELECT w.work_id,w.kind,w.case_id,w.product,w.operation_id,w.state,w.opened_at,
       a.acknowledged_by,a.acknowledged_at FROM staff_current_work w
       LEFT JOIN staff_work_acknowledgement a USING(work_id)
       WHERE w.institution=$1 AND w.product=$2 AND (w.kind='screening_review' OR a.work_id IS NULL)";
const ACKNOWLEDGED: &str =
    "SELECT a.work_id,a.kind,a.case_id,c.product,a.operation_id,c.state,a.opened_at,
       a.acknowledged_by,a.acknowledged_at FROM staff_work_acknowledgement a
       JOIN product_case c USING(case_id) WHERE c.institution=$1 AND c.product=$2";

impl Store {
    pub async fn investigations(
        &self,
        institution: &str,
        product: &str,
        acknowledged: bool,
        after: &str,
        limit: u32,
    ) -> Result<Vec<Investigation>, Error> {
        let query = format!(
            "SELECT * FROM ({}) work WHERE work_id > $3 ORDER BY work_id LIMIT $4",
            if acknowledged { ACKNOWLEDGED } else { OPEN }
        );
        sqlx::query_as(&query)
            .bind(institution)
            .bind(product)
            .bind(after)
            .bind(i64::from(limit))
            .fetch_all(&self.pool)
            .await
    }

    /// The first acknowledgement owns the receipt. Retrying it, including after the
    /// original case closed, returns that same receipt without replacing its actor.
    pub async fn acknowledge_investigation(
        &self,
        institution: &str,
        product: &str,
        work_id: &str,
        actor: &str,
    ) -> Result<Option<Investigation>, Error> {
        let mut tx = self.pool.begin().await?;
        sqlx::query("INSERT INTO staff_work_acknowledgement(work_id,case_id,kind,operation_id,opened_at,acknowledged_by)
            SELECT work_id,case_id,kind,operation_id,opened_at,$4 FROM staff_current_work
            WHERE institution=$1 AND product=$2 AND work_id=$3 ON CONFLICT(work_id) DO NOTHING")
            .bind(institution).bind(product).bind(work_id).bind(actor).execute(&mut *tx).await?;
        let query = format!("{ACKNOWLEDGED} AND a.work_id=$3");
        let receipt = sqlx::query_as(&query)
            .bind(institution)
            .bind(product)
            .bind(work_id)
            .fetch_optional(&mut *tx)
            .await?;
        tx.commit().await?;
        Ok(receipt)
    }
}

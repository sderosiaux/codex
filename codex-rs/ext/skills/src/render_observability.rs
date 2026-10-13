use crate::render::SkillMetadataBudget;
use crate::render::SkillRenderReport;

pub(crate) fn trace_catalog_budget_pressure(
    budget: SkillMetadataBudget,
    report: &SkillRenderReport,
) {
    if report.omitted_count > 0 || report.truncated_description_chars > 0 {
        tracing::info!(
            budget_limit = budget.limit(),
            total_skills = report.total_count,
            included_skills = report.included_count,
            omitted_skills = report.omitted_count,
            truncated_description_chars_per_skill = report.average_truncated_description_chars(),
            truncated_skill_descriptions = report.truncated_description_count,
            "truncated skill metadata to fit skills context budget"
        );
    }
}
